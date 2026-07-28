//! `copperclaw-cclaw` — wire protocol, client, command surface, and CLI runner
//! for the copperclaw admin socket.
//!
//! Two consumers:
//!
//! * The `cclaw` binary in this crate (see `src/main.rs`) wraps the
//!   [`commands`] layer and the [`client`] to talk to the host.
//! * The host crate (`copperclaw-host`, M5) depends on this library for the
//!   wire types so that there is exactly one source of truth for the
//!   protocol.
//!
//! See `PLAN.md` § 5.4 for the wire shape and § A2 for the subcommand
//! inventory.

#![forbid(unsafe_code)]

pub mod client;
pub mod commands;
pub mod disk;
pub mod egress;
pub mod output;
pub mod protocol;
pub mod security;
pub mod style;

pub use client::{CclawClient, ClientError, DEFAULT_TIMEOUT};
pub use commands::{ALL_COMMANDS, Cli, ParsedCall, TopCommand, default_user_socket};
pub use output::{render, render_json_pretty, render_with};
pub use protocol::{
    Caller, ErrorPayload, ProtoError, Request, Response, read_frame, read_request, read_response,
    write_frame, write_request, write_response,
};
pub use style::{Palette, color_enabled, should_colorize};

use std::process::ExitCode;

/// Name of the environment variable that switches the default caller
/// from [`Caller::Host`] to [`Caller::Agent`].
pub const AGENT_CALLER_ENV: &str = "CCLAW_AGENT_CALLER";

/// Resolve the [`Caller`] to use for a CLI invocation.
///
/// Reads `CCLAW_AGENT_CALLER` from the environment. If set, parses it as
/// JSON `{session_id, agent_group_id, messaging_group_id?}` and returns
/// [`Caller::Agent`]. Otherwise returns [`Caller::Host`]. Malformed JSON
/// yields an error so users notice misconfigured shells.
pub fn caller_from_env() -> Result<Caller, RunError> {
    let raw = std::env::var(AGENT_CALLER_ENV).ok();
    caller_from_raw(raw.as_deref())
}

/// Pure-function variant of [`caller_from_env`] used by tests. `value` is
/// the contents of the env var (or `None` if unset).
pub fn caller_from_raw(value: Option<&str>) -> Result<Caller, RunError> {
    match value {
        Some(v) if !v.trim().is_empty() => parse_agent_caller(v),
        _ => Ok(Caller::Host),
    }
}

fn parse_agent_caller(s: &str) -> Result<Caller, RunError> {
    #[derive(serde::Deserialize)]
    struct Raw {
        session: copperclaw_types::SessionId,
        agent_group: copperclaw_types::AgentGroupId,
        #[serde(default)]
        messaging_group: Option<copperclaw_types::MessagingGroupId>,
    }
    // Accept both `{session_id, agent_group_id, messaging_group_id}` (the
    // shape documented in `PLAN.md`) and the shorter `{session,
    // agent_group, messaging_group}` form.
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Either {
        Long {
            session_id: copperclaw_types::SessionId,
            agent_group_id: copperclaw_types::AgentGroupId,
            #[serde(default)]
            messaging_group_id: Option<copperclaw_types::MessagingGroupId>,
        },
        Short(Raw),
    }
    let parsed: Either = serde_json::from_str(s).map_err(|e| {
        RunError::BadAgentCaller(format!("could not parse {AGENT_CALLER_ENV} as JSON: {e}"))
    })?;
    Ok(match parsed {
        Either::Long {
            session_id,
            agent_group_id,
            messaging_group_id,
        } => Caller::Agent {
            session_id,
            agent_group_id,
            messaging_group_id,
        },
        Either::Short(Raw {
            session,
            agent_group,
            messaging_group,
        }) => Caller::Agent {
            session_id: session,
            agent_group_id: agent_group,
            messaging_group_id: messaging_group,
        },
    })
}

/// Errors surfaced by [`run_cli`].
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// CLI parsing failed; the inner error is the clap diagnostic.
    #[error("{0}")]
    ParseCli(String),
    /// The `CCLAW_AGENT_CALLER` environment variable could not be parsed.
    #[error("{0}")]
    BadAgentCaller(String),
    /// The client returned an error.
    #[error(transparent)]
    Client(#[from] ClientError),
}

/// Pluggable transport so [`run_cli`] is testable without a real socket.
#[async_trait::async_trait]
pub trait CallTransport: Send + Sync {
    /// Perform one round-trip on behalf of [`run_cli`].
    async fn call(
        &self,
        command: &str,
        args: serde_json::Value,
        caller: Caller,
    ) -> Result<serde_json::Value, ClientError>;
}

/// Default transport that dials a real Unix socket via [`CclawClient`].
pub struct SocketTransport(pub CclawClient);

#[async_trait::async_trait]
impl CallTransport for SocketTransport {
    async fn call(
        &self,
        command: &str,
        args: serde_json::Value,
        caller: Caller,
    ) -> Result<serde_json::Value, ClientError> {
        self.0.call(command, args, caller).await
    }
}

/// Result of running the CLI: an exit code and a string to emit on stdout.
#[derive(Debug)]
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: ExitCode,
}

impl RunOutput {
    pub fn success(stdout: String) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            code: ExitCode::SUCCESS,
        }
    }

    pub fn failure(stderr: String) -> Self {
        Self {
            stdout: String::new(),
            stderr,
            code: ExitCode::FAILURE,
        }
    }
}

/// Run the CLI end-to-end given a vector of argv strings and a transport.
///
/// Returns the strings to print on stdout/stderr plus the desired exit
/// code. Separated from [`crate::main`] so that integration tests can
/// drive it without spawning a subprocess.
pub async fn run_cli<I, S, T>(args: I, transport: &T) -> RunOutput
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
    T: CallTransport + ?Sized,
{
    use clap::Parser as _;
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(e) => {
            // clap renders --help / --version with exit code 0; everything
            // else is a parse error.
            let text = e.render().to_string();
            return if e.use_stderr() {
                RunOutput::failure(text)
            } else {
                RunOutput::success(text)
            };
        }
    };

    let caller = match caller_from_env() {
        Ok(c) => c,
        Err(e) => return RunOutput::failure(format!("{e}\n")),
    };

    // One styling decision for the whole invocation: never style JSON
    // (byte-identical for scripts), otherwise color only on a real
    // terminal with neither `--no-color` nor `NO_COLOR` set.
    let palette = if cli.json {
        Palette::plain()
    } else {
        Palette::new(color_enabled(cli.no_color))
    };

    let call = cli.to_call();
    // Composite client-side ops produce a `composite.*` marker command
    // that we recognise here before reaching the transport.
    if let Some(suffix) = call.command.strip_prefix("composite.") {
        return run_composite(suffix, &call.args, transport, caller, cli.json, palette).await;
    }
    match transport.call(&call.command, call.args, caller).await {
        Ok(data) => {
            let text = if cli.json {
                render_json_pretty(&data)
            } else if call.command == "usage.rollup" {
                render_usage_rollup(&data, palette)
            } else if call.command == "budgets.list" {
                render_budgets_list(&data, palette)
            } else if call.command == "sessions.list" {
                render_sessions_list(data, palette)
            } else {
                render_with(&data, palette)
            };
            let mut out = text;
            if !out.ends_with('\n') {
                out.push('\n');
            }
            RunOutput::success(out)
        }
        Err(ClientError::Remote(e)) => RunOutput::failure(format!(
            "{}\n",
            palette.error(&format!("remote error: {} ({})", e.message, e.code))
        )),
        Err(other) => RunOutput::failure(format!("{other}\n")),
    }
}

/// Dispatch a `composite.*` client-side op. Each composite fans out
/// into a sequence of real wire calls and aggregates the responses
/// into a single rendered summary.
async fn run_composite<T>(
    op: &str,
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
    palette: Palette,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    match op {
        "quickstart-cli" => run_quickstart_cli(args, transport, caller, as_json).await,
        "status" => run_status(transport, caller, as_json, palette).await,
        "health" => run_health(transport, caller, as_json, palette).await,
        "doctor" => run_doctor(args, transport, caller, as_json, palette).await,
        "security-audit" => security::run_security_audit(args, transport, caller, as_json).await,
        "completions" => run_completions(args),
        "chat" => run_chat(args, palette).await,
        "dashboard" => run_dashboard(transport, caller, as_json, palette).await,
        "groups.config-edit" => run_groups_config_edit(args, transport, caller).await,
        "egress-list-presets" => egress::run_egress_list_presets(as_json, palette),
        "egress-allow-preset" => {
            egress::run_egress_allow_preset(args, transport, caller, as_json, palette).await
        }
        "sessions-get" => run_sessions_get(args, transport, caller, as_json, palette).await,
        "sessions-tail" => run_sessions_tail(args, transport, caller, as_json, palette).await,
        other => RunOutput::failure(format!("unknown composite op: {other}\n")),
    }
}

// ---------------------------------------------------------------------------
// `cclaw usage` — token rollup table with a dollar cost column.
// ---------------------------------------------------------------------------

/// What an unpriced cost renders as: an em dash, never "$0.00". A zero
/// is a claim ("this cost nothing"); a dash is an admission ("we don't
/// know") — the host sends `cost_micros: null` for unknown models
/// precisely so surfaces can keep the two apart.
const COST_UNKNOWN: &str = "\u{2014}";

/// Format a micro-dollar amount as dollars, rounded half-up to cents.
///
/// `420_000` micros -> `"$0.42"`; `425_000` -> `"$0.43"` (half rounds
/// up). A genuine priced sub-cent value collapses to `"$0.00"` (3300
/// micros = $0.0033), which is fine *because it was priced* — rendering
/// an unknown cost is the caller's job, via [`COST_UNKNOWN`].
///
/// `pub` because the host's `daily_cost_cap` budget gate reuses it for
/// the in-channel "cost budget reached" notice, so operator-facing
/// dollar strings are formatted one way everywhere.
pub fn format_cost_dollars(micros: u64) -> String {
    let cents = micros.saturating_add(5_000) / 10_000;
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// Render the `usage.rollup` payload as the human `cclaw usage` table.
///
/// The wire payload (kept verbatim under `--json`) carries per-model
/// slices (`models`), raw `cost_micros`, and `pricing_as_of` repeated on
/// every row. The generic table renderer would dump all of that —
/// nested JSON in a cell, micros nobody can read — so this projects each
/// row down to the scalar columns, formats the group cost as dollars
/// (em dash when the host could not price every model slice), and lifts
/// `pricing_as_of` into a single footer line so stale pricing tables are
/// self-evident.
fn render_usage_rollup(data: &serde_json::Value, palette: Palette) -> String {
    let Some(rows) = data.as_array() else {
        return render_with(data, palette);
    };
    if rows.is_empty() || !rows.iter().all(serde_json::Value::is_object) {
        return render_with(data, palette);
    }
    let projected: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            // preserve_order is on workspace-wide, so insertion order
            // here is column order in the rendered table.
            let mut o = serde_json::Map::new();
            for key in [
                "agent_group_id",
                "turns",
                "input_tokens",
                "output_tokens",
                "total_tokens",
            ] {
                if let Some(v) = r.get(key) {
                    o.insert(key.to_string(), v.clone());
                }
            }
            let cost = r
                .get("cost_micros")
                .and_then(serde_json::Value::as_u64)
                .map_or_else(|| COST_UNKNOWN.to_string(), format_cost_dollars);
            o.insert("cost".to_string(), serde_json::Value::String(cost));
            for key in ["first_at", "last_at"] {
                if let Some(v) = r.get(key) {
                    o.insert(key.to_string(), v.clone());
                }
            }
            serde_json::Value::Object(o)
        })
        .collect();
    let mut out = render_with(&serde_json::Value::Array(projected), palette);
    if let Some(as_of) = rows
        .iter()
        .find_map(|r| r.get("pricing_as_of").and_then(serde_json::Value::as_str))
    {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(&palette.dim(&format!("pricing as of {as_of}")));
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// `cclaw budgets list` — caps alongside today's spend.
// ---------------------------------------------------------------------------

/// Render the `budgets.list` payload as the human `cclaw budgets list`
/// table.
///
/// The wire payload (kept verbatim under `--json`) carries the raw cap
/// columns plus today's spend (`tokens_today`, `cost_today_micros`,
/// `cost_today_unpriced_turns`) and the host-computed breach flags. The
/// generic table renderer would print micros nobody can read, so this
/// projects each row down to: dollar-formatted cost cap and spend (em
/// dash for an unset cap; a trailing `+` on the spend when some of
/// today's turns ran on unpriced models, i.e. the number is a floor),
/// and a final `state` column (`ok`, `over-tokens`, `over-cost`, or
/// `over-tokens,over-cost`) so a breached cap is visible at a glance.
fn render_budgets_list(data: &serde_json::Value, palette: Palette) -> String {
    let Some(rows) = data.as_array() else {
        return render_with(data, palette);
    };
    if rows.is_empty() || !rows.iter().all(serde_json::Value::is_object) {
        return render_with(data, palette);
    }
    let projected: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            // preserve_order is on workspace-wide, so insertion order
            // here is column order in the rendered table.
            let mut o = serde_json::Map::new();
            for key in ["agent_group_id", "daily_token_cap", "tokens_today"] {
                if let Some(v) = r.get(key) {
                    o.insert(key.to_string(), v.clone());
                }
            }
            let cost_cap = r
                .get("daily_cost_cap")
                .and_then(serde_json::Value::as_f64)
                .map_or_else(|| COST_UNKNOWN.to_string(), |d| format!("${d:.2}"));
            o.insert(
                "daily_cost_cap".to_string(),
                serde_json::Value::String(cost_cap),
            );
            let cost_today = r
                .get("cost_today_micros")
                .and_then(serde_json::Value::as_u64)
                .map_or_else(
                    || COST_UNKNOWN.to_string(),
                    |m| {
                        let mut s = format_cost_dollars(m);
                        let unpriced = r
                            .get("cost_today_unpriced_turns")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(0);
                        if unpriced > 0 {
                            // Some of today's turns could not be priced,
                            // so the dollar figure is a floor.
                            s.push('+');
                        }
                        s
                    },
                );
            o.insert(
                "cost_today".to_string(),
                serde_json::Value::String(cost_today),
            );
            for key in ["agent_turns_per_minute_cap", "agent_turns_per_hour_cap"] {
                if let Some(v) = r.get(key) {
                    o.insert(key.to_string(), v.clone());
                }
            }
            let over_tokens = r
                .get("over_daily_token_cap")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let over_cost = r
                .get("over_daily_cost_cap")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let state = match (over_tokens, over_cost) {
                (true, true) => "over-tokens,over-cost",
                (true, false) => "over-tokens",
                (false, true) => "over-cost",
                (false, false) => "ok",
            };
            o.insert(
                "state".to_string(),
                serde_json::Value::String(state.to_string()),
            );
            serde_json::Value::Object(o)
        })
        .collect();
    render_with(&serde_json::Value::Array(projected), palette)
}

// ---------------------------------------------------------------------------
// `cclaw sessions get` / `cclaw sessions tail` — per-session message rows.
// ---------------------------------------------------------------------------

/// `cclaw sessions get <id>` — one wire call (`sessions.get`), rendered
/// client-side as the session key/value table followed by the recent
/// inbound / outbound message-row tables the host attaches.
/// Render `sessions.list` with a compact parent indicator.
///
/// The host row carries the full `source_session_id` UUID (or null for
/// root sessions). A 36-char, mostly-empty column would dominate the
/// table, so the table view replaces it with a short `parent` column:
/// the first 8 chars of the parent session id, or empty for roots.
/// `--json` output is untouched (it never reaches this function), so
/// scripts keep the full UUID.
fn render_sessions_list(mut data: serde_json::Value, palette: Palette) -> String {
    if let Some(rows) = data.as_array_mut() {
        for row in rows {
            let Some(obj) = row.as_object_mut() else {
                continue;
            };
            let parent = obj
                .remove("source_session_id")
                .and_then(|v| v.as_str().map(short_session_id));
            obj.insert(
                "parent".into(),
                serde_json::Value::String(parent.unwrap_or_default()),
            );
        }
    }
    render_with(&data, palette)
}

/// First 8 chars of a session UUID — enough to disambiguate against
/// `sessions list` output while keeping the column narrow.
fn short_session_id(id: &str) -> String {
    id.chars().take(8).collect()
}

async fn run_sessions_get<T>(
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
    palette: Palette,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let Some(id) = args.get("id").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure("sessions get: missing id\n".to_string());
    };
    let data = match transport
        .call("sessions.get", serde_json::json!({"id": id}), caller)
        .await
    {
        Ok(v) => v,
        Err(ClientError::Remote(e)) => {
            return RunOutput::failure(format!(
                "{}\n",
                palette.error(&format!("remote error: {} ({})", e.message, e.code))
            ));
        }
        Err(other) => return RunOutput::failure(format!("{other}\n")),
    };
    if as_json {
        let mut out = render_json_pretty(&data);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return RunOutput::success(out);
    }
    // Pull the message-row arrays (and the W3.4 architect-state fields)
    // out so the session row renders as a clean KV table and each
    // extra gets its own section below it.
    let mut session = data;
    let (inbound, outbound, services, services_log, decisions) = match session.as_object_mut() {
        Some(o) => (
            o.remove("recent_inbound"),
            o.remove("recent_outbound"),
            o.remove("services"),
            o.remove("services_log_tail"),
            o.remove("decisions"),
        ),
        None => (None, None, None, None, None),
    };
    let mut out = render_with(&session, palette);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    for (label, rows) in [("recent inbound", inbound), ("recent outbound", outbound)] {
        let Some(rows) = rows else { continue };
        let n = rows.as_array().map_or(0, Vec::len);
        out.push('\n');
        out.push_str(&palette.header(&format!("{label} (last {n})")));
        out.push('\n');
        if n == 0 {
            out.push_str("  (none)\n");
        } else {
            out.push_str(&render_with(&rows, palette));
        }
    }
    for (label, lines) in [
        ("declared services", services),
        ("services log (tail)", services_log),
    ] {
        let Some(lines) = lines else { continue };
        out.push('\n');
        out.push_str(&palette.header(label));
        out.push('\n');
        push_line_section(&mut out, &lines);
    }
    if let Some(serde_json::Value::Array(projects)) = decisions {
        for project in &projects {
            let name = project
                .get("project")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            out.push('\n');
            out.push_str(&palette.header(&format!("decisions: {name} (tail)")));
            out.push('\n');
            match project.get("tail") {
                Some(tail) => push_line_section(&mut out, tail),
                None => out.push_str("  (none)\n"),
            }
        }
    }
    RunOutput::success(out)
}

/// Append a host-provided array of display lines as an indented block,
/// or `(none)` when the array is empty or malformed. The host already
/// sanitizes these lines (redaction, control-character stripping, char
/// caps), so this only lays them out.
fn push_line_section(out: &mut String, lines: &serde_json::Value) {
    let lines: Vec<&str> = lines
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if lines.is_empty() {
        out.push_str("  (none)\n");
        return;
    }
    for line in lines {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
}

/// Direction marker for one tail row: `<-` inbound, `->` outbound, `--`
/// for progress/status kinds (breadcrumbs, todo lists, diff cards) in
/// either direction.
fn tail_marker(direction: &str, kind: &str) -> &'static str {
    match kind {
        "breadcrumb" | "todo_list" | "diff" => "--",
        _ if direction == "in" => "<-",
        _ => "->",
    }
}

/// Render one `sessions.tail` row as a single output line.
fn format_tail_row(row: &serde_json::Value) -> String {
    let get = |k: &str| row.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
    let (direction, kind, status) = (get("direction"), get("kind"), get("status"));
    format!(
        "{}  {} {:<10} {:<9} {}",
        get("ts"),
        tail_marker(direction, kind),
        kind,
        status,
        get("preview"),
    )
}

/// `cclaw sessions tail <id> [--follow]`.
///
/// One-shot: a single `sessions.tail` call, rows printed merged and
/// time-ordered. `--follow`: prints the initial snapshot immediately to
/// stdout, then polls every second with the `since_*_seq` cursors the
/// host returns, printing only new rows — Ctrl-C to exit. Follow mode
/// streams (bypasses the buffered [`RunOutput`]) like `cclaw chat` does.
async fn run_sessions_tail<T>(
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
    palette: Palette,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    use std::io::Write as _;
    let Some(id) = args.get("id").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure("sessions tail: missing id\n".to_string());
    };
    let follow = args
        .get("follow")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if follow && as_json {
        return RunOutput::failure(
            "sessions tail: --follow streams lines and cannot honor --json\n".to_string(),
        );
    }
    let first = match transport
        .call(
            "sessions.tail",
            serde_json::json!({"id": id}),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(ClientError::Remote(e)) => {
            return RunOutput::failure(format!(
                "{}\n",
                palette.error(&format!("remote error: {} ({})", e.message, e.code))
            ));
        }
        Err(other) => return RunOutput::failure(format!("{other}\n")),
    };
    if as_json {
        let mut out = render_json_pretty(&first);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return RunOutput::success(out);
    }
    let mut out = String::new();
    let rows = first.get("rows").and_then(serde_json::Value::as_array);
    match rows {
        Some(rows) if !rows.is_empty() => {
            for row in rows {
                out.push_str(&format_tail_row(row));
                out.push('\n');
            }
        }
        _ => out.push_str("(no message rows yet)\n"),
    }
    if !follow {
        return RunOutput::success(out);
    }

    // Follow mode: emit the snapshot now, then poll with the seq
    // cursors, printing only new rows. Runs until Ctrl-C / transport
    // failure.
    print!("{out}");
    let _ = std::io::stdout().flush();
    let mut since_in = first.get("last_in_seq").and_then(serde_json::Value::as_i64);
    let mut since_out = first
        .get("last_out_seq")
        .and_then(serde_json::Value::as_i64);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let update = match transport
            .call(
                "sessions.tail",
                serde_json::json!({
                    "id": id,
                    "since_in_seq": since_in,
                    "since_out_seq": since_out,
                }),
                caller.clone(),
            )
            .await
        {
            Ok(v) => v,
            Err(ClientError::Remote(e)) => {
                return RunOutput::failure(format!(
                    "{}\n",
                    palette.error(&format!("remote error: {} ({})", e.message, e.code))
                ));
            }
            Err(other) => return RunOutput::failure(format!("{other}\n")),
        };
        if let Some(rows) = update.get("rows").and_then(serde_json::Value::as_array) {
            for row in rows {
                println!("{}", format_tail_row(row));
            }
            if !rows.is_empty() {
                let _ = std::io::stdout().flush();
            }
        }
        since_in = update
            .get("last_in_seq")
            .and_then(serde_json::Value::as_i64)
            .or(since_in);
        since_out = update
            .get("last_out_seq")
            .and_then(serde_json::Value::as_i64)
            .or(since_out);
    }
}

/// Environment variable enabling `cclaw chat`'s replay mode (any
/// non-empty value): the log is read from the START (no seek-to-end)
/// and chat exits at log EOF instead of tailing, without opening the
/// FIFO or reading stdin. Test-only escape hatch — it lets the
/// `no_ansi_when_piped` integration test drive the real rendering path
/// in a spawned binary against a fixture `chat.log` deterministically.
pub const CHAT_REPLAY_ENV: &str = "CCLAW_CHAT_REPLAY";

/// `cclaw chat` — interactive REPL against the local cli channel.
///
/// Reads lines from this terminal's stdin, writes them into the host's
/// chat FIFO, and tails `chat.log` for replies. Doesn't touch the
/// socket at all — it's pure file I/O against the install layout
/// `copperclaw-setup` produces. Exits on EOF (Ctrl-D) or Ctrl-C.
///
/// Incoming log lines follow the `CliFrame` sniff contract (a line
/// starting with `{` is a JSONL frame, anything else is legacy text)
/// and render per kind: tool breadcrumbs on a rail, todo checklists,
/// dimmed thinking, colored diffs, red errors. The tail and stdin are
/// unified under one `tokio::select!` event loop so exactly one task
/// owns the terminal — the status-line repaint can never race the
/// stdin reader. All repaint/color bytes route through the [`Palette`]
/// (`no_ansi_when_piped` is the binding constraint; piped stdout gets
/// neither ESC nor a bare `\r`). The prompt goes to stderr so
/// `cclaw chat > out.txt` keeps stdout a pure transcript.
///
/// If the FIFO is missing the host probably isn't running, so chat
/// tries to launch it via `copperclaw start` (unless `--no-autostart`
/// was passed). The retry waits up to a few seconds for the FIFO to
/// appear before giving up with the original "host not running" error.
///
/// Long-but-flat: every branch is necessary for friendly errors.
#[allow(clippy::too_many_lines)]
async fn run_chat(args: &serde_json::Value, palette: Palette) -> RunOutput {
    use std::path::PathBuf;
    use tokio::fs::OpenOptions;
    use tokio::io::{AsyncBufReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};

    let install_root = resolve_install_root();
    let resolve_path = |key: &str, default_name: &str| -> Option<PathBuf> {
        args.get(key)
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .or_else(|| install_root.as_ref().map(|r| r.join(default_name)))
    };
    let Some(log_path) = resolve_path("log", "chat.log") else {
        return RunOutput::failure(
            "cclaw chat: could not resolve install root; pass --fifo / --log\n".to_string(),
        );
    };
    let replay = std::env::var_os(CHAT_REPLAY_ENV).is_some_and(|v| !v.is_empty());
    if replay {
        return run_chat_replay(&log_path, palette).await;
    }
    let Some(fifo_path) = resolve_path("fifo", "chat.fifo") else {
        return RunOutput::failure(
            "cclaw chat: could not resolve install root; pass --fifo / --log\n".to_string(),
        );
    };
    let no_autostart = args
        .get("no_autostart")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    if !fifo_path.exists() {
        if no_autostart {
            return RunOutput::failure(format!(
                "cclaw chat: no FIFO at {} — run `copperclaw start` (or \
                 `copperclaw run`) first, or drop `--no-autostart` to let \
                 cclaw chat boot the host for you.\n",
                fifo_path.display()
            ));
        }
        match try_autostart_host(&fifo_path).await {
            AutostartResult::Booted => {}
            AutostartResult::NotFeasible(reason) => {
                return RunOutput::failure(format!(
                    "cclaw chat: no FIFO at {} and could not auto-start \
                     copperclaw ({reason}). Run `copperclaw start` (or \
                     `copperclaw run`) first.\n",
                    fifo_path.display()
                ));
            }
            AutostartResult::FailedToBoot(msg) => {
                return RunOutput::failure(format!(
                    "cclaw chat: auto-start of copperclaw failed: {msg}\n"
                ));
            }
        }
    }
    if !log_path.exists() {
        return RunOutput::failure(format!("cclaw chat: no log at {}\n", log_path.display()));
    }

    let mut fifo = match OpenOptions::new().write(true).open(&fifo_path).await {
        Ok(f) => f,
        Err(e) => {
            return RunOutput::failure(format!(
                "cclaw chat: open fifo {}: {e}\n",
                fifo_path.display()
            ));
        }
    };
    let log_file = match OpenOptions::new().read(true).open(&log_path).await {
        Ok(f) => f,
        Err(e) => {
            return RunOutput::failure(format!(
                "cclaw chat: open log {}: {e}\n",
                log_path.display()
            ));
        }
    };
    // Seek to end so we only show NEW replies, not history.
    let mut log_reader = BufReader::new(log_file);
    let _ = log_reader.seek(std::io::SeekFrom::End(0)).await;

    eprintln!(
        "cclaw chat: connected (fifo={}, log={})\n\
         type a message and press enter. Ctrl-D to exit.\n",
        fifo_path.display(),
        log_path.display()
    );

    // Two producer tasks feed channels; the select! loop below is the
    // ONLY writer to the terminal, so a status-line repaint can never
    // interleave with transcript output or the prompt. (Reading through
    // channels rather than select!-ing on `read_line` directly also
    // sidesteps `read_line`'s cancellation-unsafety.)
    let (log_tx, mut log_rx) = tokio::sync::mpsc::channel::<String>(64);
    let tail_task = tokio::spawn(async move {
        let mut reader = log_reader;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    // EOF — wait for more.
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                Ok(_) => {
                    if log_tx.send(line).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel::<String>(8);
    let stdin_task = tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin());
        loop {
            let mut line = String::new();
            match stdin.read_line(&mut line).await {
                Ok(0) => break, // EOF — dropping in_tx ends the event loop.
                Ok(_) => {
                    if in_tx.send(line).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("cclaw chat: stdin read failed: {e}");
                    break;
                }
            }
        }
    });

    let mut state = ChatRenderState::default();
    let mut printer = ChatPrinter::new(palette);
    printer.draw_bottom(state.status_line.as_deref());
    loop {
        tokio::select! {
            maybe_input = in_rx.recv() => match maybe_input {
                None => break, // stdin EOF (Ctrl-D)
                Some(line) => {
                    // The Enter keypress already moved the cursor past the
                    // old bottom line; it stays in scrollback with the
                    // user's echoed input, REPL-style. Just redraw.
                    printer.input_consumed();
                    if let Err(e) = fifo.write_all(line.as_bytes()).await {
                        eprintln!("cclaw chat: write to fifo failed: {e}");
                        break;
                    }
                    let _ = fifo.flush().await;
                    printer.draw_bottom(state.status_line.as_deref());
                }
            },
            maybe_log = log_rx.recv() => match maybe_log {
                None => break, // tail task died (log unreadable)
                Some(line) => {
                    let rendered = render_chat_line(&line, palette, &mut state);
                    printer.emit(&rendered, state.status_line.as_deref());
                }
            },
        }
    }
    printer.finish();
    tail_task.abort();
    stdin_task.abort();
    RunOutput::success(String::new())
}

/// Replay mode for `cclaw chat` (see [`CHAT_REPLAY_ENV`]): render the
/// whole log from the start through the exact same per-frame path the
/// interactive tail uses, then exit at EOF. No FIFO, no stdin.
async fn run_chat_replay(log_path: &std::path::Path, palette: Palette) -> RunOutput {
    use tokio::io::AsyncBufReadExt as _;

    let file = match tokio::fs::OpenOptions::new()
        .read(true)
        .open(log_path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            return RunOutput::failure(format!(
                "cclaw chat: open log {}: {e}\n",
                log_path.display()
            ));
        }
    };
    let mut reader = tokio::io::BufReader::new(file);
    let mut state = ChatRenderState::default();
    let mut printer = ChatPrinter::new(palette);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let rendered = render_chat_line(&line, palette, &mut state);
                printer.emit(&rendered, state.status_line.as_deref());
            }
            Err(e) => {
                return RunOutput::failure(format!(
                    "cclaw chat: read log {}: {e}\n",
                    log_path.display()
                ));
            }
        }
    }
    printer.finish();
    RunOutput::success(String::new())
}

// ---------------------------------------------------------------------------
// `cclaw chat` rendering — CliFrame JSONL -> styled transcript lines.
// ---------------------------------------------------------------------------

use copperclaw_channels_cli::CliFrame;
use copperclaw_channels_core::{
    Breadcrumb, BreadcrumbStatus, Card, DiffCard, DiffLineKind, ErrorCard, ThinkingBlock,
    TodoItemStatus, TodoList, vocab,
};

/// Identity of one rendered rail step: `(tool_name, detail)`. Status and
/// summary are deliberately excluded — an edit frame that only advances a
/// step's lifecycle must match the already-rendered step, not re-print it.
type StepKey = (String, Option<String>);

fn step_key(crumb: &Breadcrumb) -> StepKey {
    (crumb.tool_name.clone(), crumb.detail.clone())
}

/// One rail step already printed to scrollback for the current HUD
/// message, plus whether its result line has been printed yet.
struct RenderedStep {
    key: StepKey,
    result_printed: bool,
}

/// Client-side dedupe state for the chat transcript (M22 Finding 3: the
/// log is append-only edit frames; the CLIENT collapses them). Tracks
/// which rail steps are already in scrollback so a repeated HUD frame
/// prints only NEW steps, and the current bottom status line so repeats
/// are suppressed.
#[derive(Default)]
struct ChatRenderState {
    /// Steps of the current HUD (steps-carrying) breadcrumb already
    /// printed, in order.
    hud_steps: Vec<RenderedStep>,
    /// Plain (unstyled) text of the bottom status line, for dedupe.
    status_line: Option<String>,
    /// The last single (step-less) breadcrumb chip printed, so its
    /// completion edit frame prints only the result line.
    last_single: Option<(StepKey, BreadcrumbStatus)>,
}

/// Output of rendering one log line: styled transcript lines destined
/// for scrollback, plus whether the bottom status line changed.
#[derive(Debug, Default, PartialEq, Eq)]
struct RenderedChatLine {
    lines: Vec<String>,
    status_changed: bool,
}

impl RenderedChatLine {
    fn from_lines(lines: Vec<String>) -> Self {
        Self {
            lines,
            status_changed: false,
        }
    }
}

/// Sole terminal writer for `cclaw chat`. When styling is on (a real
/// TTY, no `NO_COLOR` / `--no-color`), the bottom line holds the dimmed
/// status (stdout) followed by the prompt (stderr) and is repainted in
/// place via [`Palette::repaint_prefix`]. When styling is off — which
/// includes every piped invocation — no `ESC` and no bare `\r` are ever
/// written: transcript lines print plainly, and status-line changes
/// print as ordinary deduped lines.
struct ChatPrinter {
    palette: Palette,
    /// `palette.is_colored()` — true only on a real terminal, so all
    /// repaint control bytes are gated on the same single decision as
    /// color (the `no_ansi_when_piped` contract).
    live: bool,
    /// Whether the repainted bottom line is currently on screen.
    bottom_drawn: bool,
}

impl ChatPrinter {
    fn new(palette: Palette) -> Self {
        Self {
            palette,
            live: palette.is_colored(),
            bottom_drawn: false,
        }
    }

    /// Erase the repainted bottom line if it is on screen. No-op when
    /// styling is off ([`Palette::repaint_prefix`] is empty).
    fn clear_bottom(&mut self) {
        if self.live && self.bottom_drawn {
            print!("{}", self.palette.repaint_prefix());
            self.bottom_drawn = false;
        }
    }

    /// Paint the bottom line: dimmed status text on stdout, then the
    /// prompt on stderr (stderr so `cclaw chat > out.txt` keeps stdout a
    /// pure transcript). TTY only.
    fn draw_bottom(&mut self, status: Option<&str>) {
        if !self.live {
            return;
        }
        if let Some(s) = status {
            print!("{} ", self.palette.dim(s));
        }
        let _ = std::io::Write::flush(&mut std::io::stdout());
        eprint!("> ");
        let _ = std::io::Write::flush(&mut std::io::stderr());
        self.bottom_drawn = true;
    }

    /// The user pressed Enter: the terminal echo already scrolled the
    /// old bottom line away, so just forget it (never `\r` over it).
    fn input_consumed(&mut self) {
        self.bottom_drawn = false;
    }

    /// Print one rendered log line's output: erase the bottom line,
    /// print the transcript lines, then repaint status + prompt. On a
    /// non-TTY a changed status prints as a plain deduped line instead.
    fn emit(&mut self, rendered: &RenderedChatLine, status: Option<&str>) {
        if rendered.lines.is_empty() && !rendered.status_changed {
            return; // pure duplicate frame — avoid repaint flicker
        }
        self.clear_bottom();
        for line in &rendered.lines {
            println!("{line}");
        }
        if !self.live && rendered.status_changed {
            if let Some(s) = status {
                println!("{s}");
            }
        }
        let _ = std::io::Write::flush(&mut std::io::stdout());
        self.draw_bottom(status);
    }

    /// Leave the terminal on a fresh line at exit.
    fn finish(&mut self) {
        if self.live && self.bottom_drawn {
            print!("{}", self.palette.repaint_prefix());
            let _ = std::io::Write::flush(&mut std::io::stdout());
            self.bottom_drawn = false;
        }
    }
}

/// Render one raw `chat.log` line per the `CliFrame` sniff contract:
/// a line starting with `{` is a JSONL frame, anything else is legacy
/// labelled text printed verbatim. A `{`-line that fails to parse is
/// also printed verbatim (defensive: never drop transcript content).
fn render_chat_line(line: &str, palette: Palette, state: &mut ChatRenderState) -> RenderedChatLine {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return RenderedChatLine::default();
    }
    if trimmed.starts_with('{') {
        match serde_json::from_str::<CliFrame>(trimmed) {
            Ok(frame) => return render_chat_frame(&frame, palette, state),
            Err(_) => return RenderedChatLine::from_lines(vec![trimmed.to_string()]),
        }
    }
    RenderedChatLine::from_lines(vec![trimmed.to_string()])
}

/// Render one structured frame into styled transcript lines.
fn render_chat_frame(
    frame: &CliFrame,
    palette: Palette,
    state: &mut ChatRenderState,
) -> RenderedChatLine {
    match frame {
        CliFrame::Chat { text, files } => {
            let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
            if !files.is_empty() {
                lines.push(palette.dim(&format!("[files: {}]", files.join(", "))));
            }
            RenderedChatLine::from_lines(lines)
        }
        CliFrame::Card { card } => RenderedChatLine::from_lines(render_card_lines(card, palette)),
        CliFrame::Breadcrumb { breadcrumb } => render_breadcrumb_frame(breadcrumb, palette, state),
        CliFrame::Diff { diff } => RenderedChatLine::from_lines(render_diff_lines(diff, palette)),
        CliFrame::Collapsible { text, summary, .. } => {
            let mut lines = vec![palette.dim(summary)];
            lines.extend(text.lines().map(str::to_string));
            RenderedChatLine::from_lines(lines)
        }
        CliFrame::TodoList { todo_list } => {
            RenderedChatLine::from_lines(render_todo_lines(todo_list, palette))
        }
        CliFrame::Error { error } => {
            RenderedChatLine::from_lines(render_error_lines(error, palette))
        }
        CliFrame::Thinking { thinking } => {
            RenderedChatLine::from_lines(render_thinking_lines(thinking, palette))
        }
    }
}

/// One rail step line: status-colored vocab marker + bold tool name +
/// dimmed detail. Color-off form: `⏺ shell(cargo check)`.
fn rail_step_line(crumb: &Breadcrumb, palette: Palette) -> String {
    let rail = &vocab::for_channel("cli").rail;
    let marker = rail.for_status(crumb.status);
    let marker = match crumb.status {
        BreadcrumbStatus::Running => palette.warn(marker),
        BreadcrumbStatus::Done => palette.ok(marker),
        BreadcrumbStatus::Failed => palette.fail(marker),
    };
    match &crumb.detail {
        Some(detail) => format!(
            "{marker} {}({})",
            palette.header(&crumb.tool_name),
            palette.dim(detail)
        ),
        None => format!("{marker} {}", palette.header(&crumb.tool_name)),
    }
}

/// One rail result line under a step. Color-off form: `  ⎿ passed in 3.1s`.
fn rail_result_line(summary: &str, status: BreadcrumbStatus, palette: Palette) -> String {
    let rail = &vocab::for_channel("cli").rail;
    let text = if status == BreadcrumbStatus::Failed {
        palette.fail(summary)
    } else {
        palette.dim(summary)
    };
    format!("  {} {text}", palette.dim(rail.result_leader))
}

/// Render a breadcrumb frame with client-side dedupe.
///
/// A steps-carrying frame is a HUD aggregate that arrives repeatedly as
/// edit frames: print only steps not yet in scrollback (plus late result
/// lines for steps that have since completed) and update the bottom
/// status line from the top-level summary. A step-less frame is a single
/// tool chip: its completion edit prints only the result line.
fn render_breadcrumb_frame(
    crumb: &Breadcrumb,
    palette: Palette,
    state: &mut ChatRenderState,
) -> RenderedChatLine {
    let mut lines = Vec::new();
    if crumb.steps.is_empty() {
        let key = step_key(crumb);
        match &state.last_single {
            // Duplicate edit frame — same chip, same lifecycle. Skip.
            Some((k, st)) if *k == key && *st == crumb.status => {}
            // Completion edit of the chip already in scrollback: print
            // only the result line.
            Some((k, BreadcrumbStatus::Running)) if *k == key => {
                let summary = crumb.summary.as_deref().unwrap_or(crumb.status.as_str());
                lines.push(rail_result_line(summary, crumb.status, palette));
                state.last_single = Some((key, crumb.status));
            }
            _ => {
                lines.push(rail_step_line(crumb, palette));
                if crumb.status != BreadcrumbStatus::Running {
                    if let Some(summary) = &crumb.summary {
                        lines.push(rail_result_line(summary, crumb.status, palette));
                    }
                }
                state.last_single = Some((key, crumb.status));
            }
        }
        return RenderedChatLine::from_lines(lines);
    }

    // HUD aggregate. A frame whose steps don't extend the rendered
    // prefix is a NEW logical HUD message — reset and render from
    // scratch.
    let extends_prefix = state.hud_steps.len() <= crumb.steps.len()
        && state
            .hud_steps
            .iter()
            .zip(&crumb.steps)
            .all(|(rendered, step)| rendered.key == step_key(step));
    if !extends_prefix {
        state.hud_steps.clear();
    }
    // Late result lines for already-rendered steps that have completed.
    for (rendered, step) in state.hud_steps.iter_mut().zip(&crumb.steps) {
        if !rendered.result_printed {
            if let Some(summary) = &step.summary {
                lines.push(rail_result_line(summary, step.status, palette));
                rendered.result_printed = true;
            }
        }
    }
    // New steps.
    for step in &crumb.steps[state.hud_steps.len()..] {
        lines.push(rail_step_line(step, palette));
        let mut result_printed = false;
        if let Some(summary) = &step.summary {
            lines.push(rail_result_line(summary, step.status, palette));
            result_printed = true;
        }
        state.hud_steps.push(RenderedStep {
            key: step_key(step),
            result_printed,
        });
    }
    // Bottom status line from the collapsed top-level summary.
    let status_text = crumb
        .summary
        .clone()
        .unwrap_or_else(|| match &crumb.detail {
            Some(detail) => format!("{}({detail})", crumb.tool_name),
            None => crumb.tool_name.clone(),
        });
    let status_changed = state.status_line.as_deref() != Some(status_text.as_str());
    if status_changed {
        state.status_line = Some(status_text);
    }
    RenderedChatLine {
        lines,
        status_changed,
    }
}

/// Card: bold title, body lines, `label: value` fields.
fn render_card_lines(card: &Card, palette: Palette) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(title) = &card.title {
        lines.push(palette.header(title));
    }
    if let Some(body) = &card.body {
        lines.extend(body.lines().map(str::to_string));
    }
    for field in &card.fields {
        lines.push(format!("{}: {}", palette.header(&field.label), field.value));
    }
    lines
}

/// Todo checklist: ASCII checkbox glyphs from the cli vocab binding,
/// strikethrough for completed items, blocked reason appended.
fn render_todo_lines(todo: &TodoList, palette: Palette) -> Vec<String> {
    let glyphs = &vocab::for_channel("cli").todo;
    let mut lines = Vec::new();
    if let Some(title) = &todo.title {
        lines.push(palette.header(title));
    }
    for item in &todo.items {
        let glyph = glyphs.get(item.status);
        let text = match item.status {
            TodoItemStatus::Completed => palette.strike(&item.text),
            TodoItemStatus::InProgress | TodoItemStatus::Blocked => item.text.clone(),
            TodoItemStatus::Pending => palette.dim(&item.text),
        };
        let mut line = format!("{glyph} {text}");
        if item.status == TodoItemStatus::Blocked {
            if let Some(reason) = item.blocked_reason_text() {
                line.push_str(&palette.dim(&format!(" — {reason}")));
            }
        }
        lines.push(line);
    }
    lines
}

/// Diff: bold `path (+A -R)` header, dimmed hunk markers, green added /
/// red removed lines with unified-diff prefixes.
fn render_diff_lines(diff: &DiffCard, palette: Palette) -> Vec<String> {
    let mut lines = vec![palette.header(&format!(
        "{} (+{} -{})",
        diff.path, diff.added, diff.removed
    ))];
    for hunk in &diff.hunks {
        lines.push(palette.dim(&format!(
            "@@ -{},{} +{},{} @@",
            hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
        )));
        for line in &hunk.lines {
            lines.push(match line.kind {
                DiffLineKind::Add => palette.diff_add(&format!("+{}", line.text)),
                DiffLineKind::Remove => palette.diff_remove(&format!("-{}", line.text)),
                DiffLineKind::Context => format!(" {}", line.text),
            });
        }
    }
    if diff.truncated {
        lines.push(palette.dim("[diff truncated]"));
    }
    lines
}

/// Error card: red headline, dimmed details, retry footer.
fn render_error_lines(error: &ErrorCard, palette: Palette) -> Vec<String> {
    let mut lines = vec![palette.fail(&format!(
        "[{} error] {}: {}",
        error.kind.label(),
        error.title,
        error.summary
    ))];
    if let Some(details) = &error.details {
        lines.extend(details.lines().map(|l| palette.dim(l)));
    }
    if error.retryable {
        lines.push(palette.dim("(will retry automatically)"));
    }
    lines
}

/// Thinking block: dimmed text; redacted blocks show a placeholder.
fn render_thinking_lines(thinking: &ThinkingBlock, palette: Palette) -> Vec<String> {
    if thinking.redacted {
        return vec![palette.dim("[thinking redacted]")];
    }
    thinking.text.lines().map(|l| palette.dim(l)).collect()
}

/// Outcome of [`try_autostart_host`].
enum AutostartResult {
    /// `copperclaw start` returned cleanly and the FIFO is now present.
    Booted,
    /// We never even tried — the binary isn't on PATH or something
    /// else made autostart inapplicable.
    NotFeasible(String),
    /// `copperclaw start` was invoked but failed (non-zero exit, FIFO
    /// never appeared, etc.). The string is operator-facing.
    FailedToBoot(String),
}

/// Try to launch the host via `copperclaw start` and wait for the chat
/// FIFO to appear. Used by `cclaw chat` when the FIFO is missing.
///
/// Returns one of:
/// - `Booted` once the FIFO is visible.
/// - `NotFeasible(...)` if we can't locate `copperclaw` on PATH.
/// - `FailedToBoot(...)` if the spawn itself failed or the FIFO
///   didn't appear within the start grace window.
async fn try_autostart_host(fifo_path: &std::path::Path) -> AutostartResult {
    let Some(exe) = which_copperclaw() else {
        return AutostartResult::NotFeasible("copperclaw not on PATH".to_string());
    };
    eprintln!(
        "cclaw chat: host not running; starting it via `{}`...",
        exe.display()
    );
    let out = tokio::process::Command::new(&exe)
        .arg("start")
        .output()
        .await;
    let out = match out {
        Ok(o) => o,
        Err(e) => return AutostartResult::FailedToBoot(format!("spawn {}: {e}", exe.display())),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        return AutostartResult::FailedToBoot(if stderr.is_empty() {
            format!("`copperclaw start` exited {}", out.status)
        } else {
            stderr.trim().to_string()
        });
    }
    // `copperclaw start` returns once the *admin* socket is up, but the
    // chat FIFO is created by the cli channel during boot — usually
    // earlier than the socket, but we poll a short grace window to
    // make sure it's landed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if fifo_path.exists() {
            return AutostartResult::Booted;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    AutostartResult::FailedToBoot(format!(
        "host started but FIFO did not appear at {}",
        fifo_path.display()
    ))
}

/// Locate `copperclaw` on `PATH`. Returns `None` if not found.
fn which_copperclaw() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("copperclaw");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Platform-default copperclaw install root. Mirrors the resolver in
/// `copperclaw-host::config::default_install_env_file` / setup's
/// `default_data_dir_for` so chat's defaults agree with where setup
/// put things.
fn resolve_install_root() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
    let os = std::env::consts::OS;
    Some(match os {
        "macos" => home
            .join("Library")
            .join("Application Support")
            .join("copperclaw"),
        "linux" => std::env::var_os("XDG_DATA_HOME")
            .map(std::path::PathBuf::from)
            .filter(|x| !x.as_os_str().is_empty())
            .map_or_else(
                || home.join(".local").join("share").join("copperclaw"),
                |xdg| xdg.join("copperclaw"),
            ),
        _ => home.join(".copperclaw"),
    })
}

/// Key names with a non-empty value in the install's `.env`, for doctor's
/// env-var courtesy checks. Presence only — values are never read out of
/// this function. Tolerates `export KEY=...`, quoted values, comments, and
/// a missing file (empty set).
fn install_env_nonempty_keys() -> std::collections::HashSet<String> {
    let Some(root) = resolve_install_root() else {
        return std::collections::HashSet::new();
    };
    let Ok(body) = std::fs::read_to_string(root.join(".env")) else {
        return std::collections::HashSet::new();
    };
    body.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let value = value.trim().trim_matches('"').trim_matches('\'');
            (!value.is_empty()).then(|| key.trim().to_string())
        })
        .collect()
}

/// `cclaw health` — one-shot operator probe. Lists session-state
/// counts (running / idle / stopped) and the last 5 audit entries
/// so the operator can see at a glance whether the host is alive
/// and whether anything has recently mutated state. Designed to be
/// cheap and side-effect-free; a real `/healthz` HTTP endpoint can
/// reuse the same data.
async fn run_health<T>(transport: &T, caller: Caller, as_json: bool, palette: Palette) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let sessions_all = match transport
        .call("sessions.list", serde_json::json!({}), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("sessions.list", &e)),
    };
    let sessions_active = match transport
        .call(
            "sessions.list",
            serde_json::json!({"status": "active"}),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("sessions.list", &e)),
    };
    let audit = match transport
        .call(
            "audit.list",
            serde_json::json!({"since": "24h", "limit": 5}),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("audit.list", &e)),
    };
    let dropped = match transport
        .call("dropped-messages.list", serde_json::json!({}), caller)
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("dropped-messages.list", &e)),
    };

    let mut running = 0usize;
    let mut idle = 0usize;
    let mut stopped = 0usize;
    if let Some(rows) = sessions_active.as_array() {
        for r in rows {
            match r
                .get("container_status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
            {
                "running" => running += 1,
                "idle" => idle += 1,
                "stopped" => stopped += 1,
                _ => {}
            }
        }
    }

    if as_json {
        let summary = serde_json::json!({
            "sessions_total": array_len(&sessions_all),
            "sessions_active": array_len(&sessions_active),
            "sessions_running": running,
            "sessions_idle": idle,
            "sessions_stopped": stopped,
            "dropped_messages": array_len(&dropped),
            "recent_audit": audit,
        });
        let mut out = render_json_pretty(&summary);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return RunOutput::success(out);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "sessions total:    {}\n",
        array_len(&sessions_all)
    ));
    out.push_str(&format!(
        "sessions active:   {}\n",
        array_len(&sessions_active)
    ));
    out.push_str(&format!("  running:         {running}\n"));
    out.push_str(&format!("  idle:            {idle}\n"));
    out.push_str(&format!("  stopped:         {stopped}\n"));
    out.push_str(&format!("dropped messages:  {}\n", array_len(&dropped)));
    out.push('\n');
    if array_len(&audit) > 0 {
        out.push_str("recent mutations (24h, up to 5)\n");
        out.push_str(&render_with(&audit, palette));
        out.push('\n');
    } else {
        out.push_str("recent mutations (24h): none\n");
    }
    RunOutput::success(out)
}

/// Severity of a single `doctor` check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckLevel {
    Ok,
    Warn,
    Fail,
}

impl CheckLevel {
    fn tag(self) -> &'static str {
        match self {
            Self::Ok => "OK   ",
            Self::Warn => "WARN ",
            Self::Fail => "FAIL ",
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One result from `cclaw doctor`. `fix` is shown to the operator
/// only when level != Ok, so an all-green report stays short.
#[derive(Debug, Clone)]
struct Check {
    name: &'static str,
    level: CheckLevel,
    detail: String,
    /// Optional shell command the operator can run to remediate.
    fix: Option<String>,
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            level: CheckLevel::Ok,
            detail: detail.into(),
            fix: None,
        }
    }
    fn warn(name: &'static str, detail: impl Into<String>, fix: Option<&str>) -> Self {
        Self {
            name,
            level: CheckLevel::Warn,
            detail: detail.into(),
            fix: fix.map(String::from),
        }
    }
    fn fail(name: &'static str, detail: impl Into<String>, fix: Option<&str>) -> Self {
        Self {
            name,
            level: CheckLevel::Fail,
            detail: detail.into(),
            fix: fix.map(String::from),
        }
    }
    fn to_json(&self) -> serde_json::Value {
        let mut o = serde_json::Map::new();
        o.insert("name".into(), serde_json::Value::String(self.name.into()));
        o.insert(
            "level".into(),
            serde_json::Value::String(self.level.as_str().into()),
        );
        o.insert(
            "detail".into(),
            serde_json::Value::String(self.detail.clone()),
        );
        if let Some(f) = &self.fix {
            o.insert("fix".into(), serde_json::Value::String(f.clone()));
        }
        serde_json::Value::Object(o)
    }
}

/// `cclaw doctor` — composite first-run diagnostic. Walks the install
/// end-to-end and reports per-check status plus a `fix:` line on each
/// non-OK row so an operator can copy-paste their way to a working
/// install.
///
/// Sequence:
/// 1. `groups.list` — central DB reachable through the host socket.
///    Subsumes "host process running" + "socket file present" +
///    "central DB readable".
/// 2. Group / wiring counts — gating for `cclaw chat` to do
///    anything useful.
/// 3. Recent audit errors — surfacing flapping mutations.
/// 4. Dropped-message backlog.
/// 5. Env-var sanity — `ANTHROPIC_API_KEY` present, plus a courtesy
///    note about which `web_search` providers are wired up.
///
/// The transport calls run sequentially because the cclaw socket is
/// strictly single-flight per connection; ~5 round-trips total against
/// a Unix socket is sub-millisecond in practice.
#[allow(clippy::too_many_lines)] // Sequential checks are easier to follow inline.
async fn run_doctor<T>(
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
    palette: Palette,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let _ = args; // doctor takes no arguments at present
    let mut checks: Vec<Check> = Vec::new();

    // 1. Host reachable + central DB readable.
    let groups = match transport
        .call("groups.list", serde_json::json!({}), caller.clone())
        .await
    {
        Ok(v) => {
            checks.push(Check::ok(
                "host-reachable",
                "cclaw socket responded; central DB is readable",
            ));
            v
        }
        Err(e) => {
            checks.push(Check::fail(
                "host-reachable",
                format!("could not reach the host socket: {e}"),
                Some("start the host: `copperclaw run` (or `systemctl start copperclaw` if installed as a service)"),
            ));
            return finalise_doctor(&checks, as_json, palette);
        }
    };

    // 2. At least one agent group must exist for any of the messaging
    //    paths to fire.
    let group_count = array_len(&groups);
    if group_count == 0 {
        checks.push(Check::fail(
            "agent-group",
            "no agent groups configured — there is no one for inbound messages to route to",
            Some("create the default group: `cclaw quickstart cli --name first`"),
        ));
    } else {
        checks.push(Check::ok(
            "agent-group",
            format!("{group_count} group(s) configured"),
        ));
    }

    // 3. Wirings — messaging-group ↔ agent-group bindings.
    let wirings = transport
        .call("wirings.list", serde_json::json!({}), caller.clone())
        .await
        .ok();
    match wirings.as_ref().map(array_len) {
        Some(0) => checks.push(Check::warn(
            "wiring",
            "no messaging-group wirings — agent groups exist but nothing routes inbound to them",
            Some("`cclaw quickstart cli --name first` creates the default group + wiring in one call"),
        )),
        Some(n) => checks.push(Check::ok("wiring", format!("{n} wiring(s) configured"))),
        None => checks.push(Check::warn(
            "wiring",
            "could not list wirings",
            Some("re-run `cclaw doctor` after the host comes back; check stderr for `wirings.list` errors"),
        )),
    }

    // 4. Sessions snapshot (informational, never fatal — having zero
    //    sessions is the steady state for a brand-new install).
    if let Ok(active) = transport
        .call(
            "sessions.list",
            serde_json::json!({"status": "active"}),
            caller.clone(),
        )
        .await
    {
        let n = array_len(&active);
        if n > 0 {
            checks.push(Check::ok("sessions", format!("{n} active session(s)")));
        } else {
            checks.push(Check::ok(
                "sessions",
                "no active sessions yet — send a message via `cclaw chat` to create one",
            ));
        }
    }

    // 5. Recent audit failures — anything that flapped in the last
    //    hour points the operator at the broken command.
    if let Ok(audit) = transport
        .call(
            "audit.list",
            serde_json::json!({"since": "1h", "limit": 50}),
            caller.clone(),
        )
        .await
    {
        let bad: Vec<&serde_json::Value> = audit
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|r| {
                        r.get("result").and_then(serde_json::Value::as_str) == Some("error")
                    })
                    .collect()
            })
            .unwrap_or_default();
        if bad.is_empty() {
            checks.push(Check::ok(
                "audit-errors",
                "no failed mutations in the last hour",
            ));
        } else {
            let sample: Vec<String> = bad
                .iter()
                .take(3)
                .map(|r| {
                    let cmd = r
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let code = r
                        .get("error_code")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    format!("{cmd} -> {code}")
                })
                .collect();
            checks.push(Check::warn(
                "audit-errors",
                format!(
                    "{} failed mutation(s) in the last hour: {}",
                    bad.len(),
                    sample.join(", ")
                ),
                Some("`cclaw audit list --since 1h` to see the full set"),
            ));
        }
    }

    // 6. Dropped-message backlog.
    if let Ok(dropped) = transport
        .call(
            "dropped-messages.list",
            serde_json::json!({}),
            caller.clone(),
        )
        .await
    {
        let n = array_len(&dropped);
        if n == 0 {
            checks.push(Check::ok("dropped-messages", "no dropped inbound messages"));
        } else {
            checks.push(Check::warn(
                "dropped-messages",
                format!("{n} dropped inbound message(s)"),
                Some("`cclaw dropped-messages list` to inspect; usually means a sender hit the approval gate"),
            ));
        }
    }

    // 7. Env-var sanity (local checks, no transport call). The host reads
    //    its keys from its own env / the install's `.env` at boot — the
    //    cclaw shell's env is only a proxy. Check BOTH so a key that lives
    //    only in `.env` (the normal setup-written case) reads as configured
    //    instead of producing a misleading "unset" (found live 2026-07-16:
    //    a configured TAVILY_API_KEY reported as "no providers").
    let env_file_keys = install_env_nonempty_keys();
    if std::env::var("ANTHROPIC_API_KEY")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        checks.push(Check::ok(
            "anthropic-key",
            "ANTHROPIC_API_KEY is set in this shell's env",
        ));
    } else if env_file_keys.contains("ANTHROPIC_API_KEY") {
        checks.push(Check::ok(
            "anthropic-key",
            "ANTHROPIC_API_KEY is set in the install's .env (the host reads it at boot)",
        ));
    } else {
        checks.push(Check::warn(
            "anthropic-key",
            "ANTHROPIC_API_KEY is unset in this shell and in the install's .env — the host may still have it from the env it was launched with",
            Some("set ANTHROPIC_API_KEY in the install's .env (typically under $XDG_DATA_HOME/copperclaw/.env on Linux)"),
        ));
    }

    // 8. Web-search providers (informational only — none configured
    //    is a perfectly valid install). Keys in the install's `.env`
    //    count: the host forwards them into session containers at spawn.
    let providers: Vec<String> = [
        ("TAVILY_API_KEY", "tavily"),
        ("EXA_API_KEY", "exa"),
        ("BRAVE_SEARCH_API_KEY", "brave"),
        ("SERPAPI_API_KEY", "serpapi"),
    ]
    .iter()
    .filter_map(|(var, name)| {
        let in_shell = std::env::var(var).ok().filter(|s| !s.is_empty()).is_some();
        if in_shell {
            Some(format!("{name} (shell env)"))
        } else if env_file_keys.contains(*var) {
            Some(format!("{name} (.env)"))
        } else {
            None
        }
    })
    .collect();
    if providers.is_empty() {
        checks.push(Check::ok(
            "web-search",
            "no web_search providers configured in this shell or the install's .env (the tool will surface a friendly error if the agent calls it)",
        ));
    } else {
        checks.push(Check::ok(
            "web-search",
            format!(
                "{} provider(s) configured: {}",
                providers.len(),
                providers.join(", ")
            ),
        ));
    }

    // 9. Egress posture (Phase 0a v1 / Top 10 #6). Surface the host egress
    //    mode and each group's effective allow-list so the operator can see,
    //    in one place, that deny-default (when enabled) carries the
    //    auto-injected model endpoint and won't blackhole model traffic.
    if let Ok(egress) = transport
        .call("egress.status", serde_json::json!({}), caller.clone())
        .await
    {
        let mode = egress
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("allow-all");
        let model = egress
            .get("model_endpoint")
            .and_then(serde_json::Value::as_str);
        let group_n = egress.get("groups").map_or(0, array_len);
        let dns_filter = egress
            .get("dns_filter")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let nft_status = egress
            .get("nft_status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unsupported");
        let model_note = model.map_or_else(
            || {
                " (no model endpoint auto-injected — ANTHROPIC_BASE_URL unset on the host)"
                    .to_string()
            },
            |m| format!(" (model endpoint auto-injected: {m})"),
        );
        if mode == "allow-all" {
            checks.push(Check::ok(
                "egress",
                format!(
                    "egress mode: allow-all (default; per-group allow-lists advisory only){model_note}; {group_n} group(s)"
                ),
            ));
        } else {
            // deny-default (v2): DNS filtering is enforced (resolv.conf pinned
            // to a resolver answering only the effective allow-list); the
            // empty-allow `network_mode: none` cut is enforced; the per-session
            // nftables L3/L4 ruleset is constructed and its privileged netns
            // apply is gated on `nft_status`. Warn (not fail): it's an
            // intentional opt-in posture, but the operator should know which
            // layers are live vs. deferred.
            let dns_note = if dns_filter {
                "DNS filtering ON (resolv.conf pinned read-only to the filter resolver; the resolver answers only the allow-list and NXDOMAINs the rest — the filter-resolver sidecar is the runtime piece)"
            } else {
                "DNS filtering OFF"
            };
            let nft_note = match nft_status {
                "available" => {
                    "nftables ruleset constructed; netns apply deferred (needs container PID + CAP_NET_ADMIN)"
                }
                "tool-missing" => {
                    "nftables NOT applied (`nft` not on host PATH); ruleset constructed"
                }
                _ => "nftables NOT applied (unsupported platform); ruleset constructed",
            };
            checks.push(Check::warn(
                "egress",
                format!(
                    "egress mode: deny-default (opt-in){model_note}; {group_n} group(s). \
                     {dns_note}. {nft_note}. bollard hard-cuts the network when a group's \
                     effective allow-list is empty"
                ),
                Some("`cclaw egress` shows each group's effective allow-list; set per-group entries with `cclaw groups config set-egress-allow`. Install `nftables` + run the host with CAP_NET_ADMIN to enable L3/L4 filtering"),
            ));
        }
    }

    // 10. Free disk on the filesystem holding the install's data dir.
    //     Local check (no transport). The host degrades *silently* when
    //     the root fs fills — `tasks_snapshot` write failures, container
    //     spawns and test suites failing on ENOSPC — and before this row
    //     `cclaw doctor` reported all-OK straight through a real 100%-full
    //     incident. This closes that gap.
    checks.push(disk_space_check());

    // ---------------------------------------------------------------------
    // M21 O1 — "doctor learns everything Wave 1 recovers from". Each of the
    // rows below detects a failure mode that the M21 stability program adds
    // a *recovery* path for; before this card an operator running `cclaw
    // doctor` against a host with (say) a dead delivery loop or a corrupt
    // per-session DB got an all-green report. Every new FAIL prints a `fix:`
    // line naming a real command (house rule). The new transport calls are
    // deliberately issued AFTER the existing ones so the pre-existing
    // sequential-transport tests still map their seeded responses to the
    // original checks; every new check degrades to a skip/WARN (never a
    // FAIL) when its transport call errors, so a legacy host that lacks a
    // handler reads clean rather than alarming.

    // 11. Container-runtime reachability. The host talks to Docker over its
    //     socket (bollard, `Docker::connect_with_socket_defaults`); if the
    //     daemon is down, every container spawn fails and no session can
    //     start. cclaw is co-located with the host, so we probe the same
    //     socket directly.
    checks.push(runtime_check_from(&probe_runtime()));

    // 12. Background-loop liveness + host degraded mode (M21 S1 supervisor).
    //     Read via the `host.status` admin-socket handler. A dead or
    //     degraded loop is invisible to logs-at-a-glance — this is the row
    //     that catches it. `unavailable` (a host build without the
    //     supervisor) and any transport error are treated as skip, not a
    //     crash, per the O1 card.
    // `Err` (no supervisor / legacy host / unreachable) is skipped, not a
    // crash, per the O1 card.
    if let Ok(status) = transport
        .call("host.status", serde_json::json!({}), caller.clone())
        .await
    {
        checks.push(host_loops_check(&status));
    }

    // 13. Stuck / heartbeat-stale sessions. The runner touches
    //     `<session_root>/.heartbeat` continuously while alive; a *running*
    //     session whose heartbeat has gone stale is exactly the wedged /
    //     crashed-runner case S1/S2 recover from. We list running sessions
    //     over the socket, then stat their heartbeat files locally (cclaw is
    //     co-located). Skipped whole if the runtime data root can't be
    //     resolved or the session list errors.
    if let Some(data_root) = doctor_data_root() {
        if let Ok(running) = transport
            .call(
                "sessions.list",
                serde_json::json!({"status": "running"}),
                caller.clone(),
            )
            .await
        {
            let rows = running.as_array().cloned().unwrap_or_default();
            checks.push(stuck_sessions_check(
                &data_root,
                &rows,
                std::time::SystemTime::now(),
            ));
        }
    }

    // 14. Provider-chain health (M16 Phase 4 failover, made live by O3). An
    //     EMPTY chain is the inert "no resilience configured" default — a
    //     WARN with a config pointer, never a FAIL. A configured chain whose
    //     every entry is currently unhealthy (degraded + still in cooldown)
    //     IS a FAIL: the group has no provider it can reach.
    {
        let now = chrono::Utc::now();
        let mut states: Vec<(String, ChainState)> = Vec::new();
        if let Some(rows) = groups.as_array() {
            for g in rows {
                let Some(id) = g.get("id").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                if let Ok(status) = transport
                    .call(
                        "groups.provider.status",
                        serde_json::json!({"id": id}),
                        caller.clone(),
                    )
                    .await
                {
                    states.push((id.to_string(), group_chain_state(&status, now)));
                }
            }
        }
        if let Some(check) = provider_chain_check(&states) {
            checks.push(check);
        }
    }

    // 15. Dead-letter / `no_adapter` outbound backlog (M21 S3/S5). Outbound
    //     rows whose channel has no live adapter, or that exhausted their
    //     delivery retries, land in the outbound dead-letter table. They are
    //     recoverable ONLY by an explicit replay (auto-replay is a standing
    //     rejection), so a non-empty backlog is a FAIL pointing at the
    //     replay command.
    if let Ok(dl) = transport
        .call(
            "dropped-messages.outbound-list",
            serde_json::json!({}),
            caller.clone(),
        )
        .await
    {
        checks.push(dead_letter_check(&dl));
    }

    // 16. DB integrity (M21 O2). O2 quarantines a corrupt per-session DB by
    //     writing a `.quarantined` sidecar next to it and excluding the
    //     session from sweeps; a quarantined session is otherwise silently
    //     dead. Doctor is the surface O2 relies on to make that visible: we
    //     scan the session tree for the sidecar and also `quick_check` the
    //     central DB. Local filesystem + read-only SQLite — no transport.
    if let Some(data_root) = doctor_data_root() {
        checks.push(db_integrity_check(&data_root));
    }

    // 17. Budget caps (M24 U2). A breached daily token or dollar cap
    //     defers every spawn for that group until UTC midnight — from the
    //     outside it looks like the agent went silent, so surface it
    //     here. The host computes the breach flags in `budgets.list`
    //     with the same rollup the spawn gate compares against. An older
    //     host (no flags in the payload) or a transport error skips the
    //     row rather than guessing.
    if let Ok(budgets) = transport
        .call("budgets.list", serde_json::json!({}), caller.clone())
        .await
    {
        checks.push(budgets_check(&budgets));
    }

    finalise_doctor(&checks, as_json, palette)
}

/// Doctor row for check 17: which groups are over a daily (token or
/// dollar) cap, per the host-computed `over_daily_token_cap` /
/// `over_daily_cost_cap` flags in the `budgets.list` payload.
fn budgets_check(budgets: &serde_json::Value) -> Check {
    let over: Vec<String> = budgets
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let over_tokens = r
                        .get("over_daily_token_cap")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let over_cost = r
                        .get("over_daily_cost_cap")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    if !(over_tokens || over_cost) {
                        return None;
                    }
                    let ag = r
                        .get("agent_group_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let which = match (over_tokens, over_cost) {
                        (true, true) => "token+cost caps",
                        (true, false) => "token cap",
                        _ => "cost cap",
                    };
                    Some(format!("{ag} ({which})"))
                })
                .collect()
        })
        .unwrap_or_default();
    if over.is_empty() {
        Check::ok("budgets", "no agent group is over its daily token/cost cap")
    } else {
        Check::warn(
            "budgets",
            format!(
                "{} group(s) over a daily cap (spawns deferred until UTC midnight): {}",
                over.len(),
                over.join(", ")
            ),
            Some(
                "raise or clear the cap: `cclaw budgets set --agent-group-id <id> --daily-tokens N` / `--daily-cost N` (0 removes the cap)",
            ),
        )
    }
}

/// The thresholds, the pure classifier, and the `statvfs` call now live in
/// [`crate::disk`] — the host's spawn preflight
/// (`copperclaw_host::container_manager::host_resources`) classifies free
/// space with the *same* numbers, and two copies would drift into `doctor`
/// saying OK while the reconcile loop refuses every spawn. What stays here
/// is only the doctor-specific presentation.
use crate::disk::{DISK_FIX, DiskLevel, free_pct, human_bytes};

/// `disk-space` doctor severity from free bytes. Thin adapter over
/// [`crate::disk::level`] mapping the shared band onto doctor's own
/// [`CheckLevel`].
fn disk_level(free_bytes: u64, total_bytes: u64) -> CheckLevel {
    match crate::disk::level(free_bytes, total_bytes) {
        DiskLevel::Ok => CheckLevel::Ok,
        DiskLevel::Warn => CheckLevel::Warn,
        DiskLevel::Fail => CheckLevel::Fail,
    }
}

/// One binary gibibyte — test-only alias so the existing boundary tests
/// keep reading in GiB. Non-test code goes through [`crate::disk`].
#[cfg(test)]
const GIB: u64 = crate::disk::GIB;

// Test seam: when set on the current thread, `disk_space_check` skips the
// real `statvfs` and reports these synthetic `(free, total)` bytes. Keeps
// the transport-seeded doctor tests deterministic (independent of the test
// host's actual free space) and lets tests drive the WARN/FAIL rendering
// through the full doctor flow. Thread-local, so parallel tests don't race;
// only ever compiled in test builds.
#[cfg(test)]
thread_local! {
    static DISK_OVERRIDE: std::cell::Cell<Option<(u64, u64)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn set_disk_override(free_bytes: u64, total_bytes: u64) {
    DISK_OVERRIDE.with(|c| c.set(Some((free_bytes, total_bytes))));
}

/// Build the `disk-space` doctor row. Resolves the install's data dir
/// client-side (cclaw runs on the same box as the host), stats the
/// filesystem holding it, and classifies free space. Degrades to a WARN
/// row — never a panic, never a block — when the path can't be resolved
/// or `statvfs` fails, so a stat error can't take down the rest of doctor.
fn disk_space_check() -> Check {
    // In test builds, default to a healthy synthetic filesystem so the
    // transport-focused doctor tests don't hinge on the host's real disk;
    // individual tests override via `set_disk_override`.
    #[cfg(test)]
    {
        let (free, total) = DISK_OVERRIDE
            .with(std::cell::Cell::get)
            .unwrap_or((500 * GIB, 1024 * GIB));
        disk_check_bytes(free, total, std::path::Path::new("<test>"))
    }
    #[cfg(not(test))]
    {
        let Some(root) = resolve_install_root() else {
            return Check::warn(
                "disk-space",
                "could not resolve the install root to stat free disk space",
                Some("set $HOME (or XDG_DATA_HOME on Linux) so cclaw can locate the install"),
            );
        };
        // Prefer the data dir; on a fresh box it may not exist yet, so fall
        // back to the nearest existing ancestor (the mount is the same).
        let data_dir = root.join("data");
        let mut stat_path = data_dir.clone();
        while !stat_path.exists() {
            match stat_path.parent() {
                Some(parent) => stat_path = parent.to_path_buf(),
                None => break,
            }
        }
        disk_check_at(&stat_path, &data_dir)
    }
}

/// Format a classified `(free, total)` pair into a `disk-space` [`Check`].
/// The pure rendering half — no I/O — shared by the real `statvfs` path
/// and the test seam.
fn disk_check_bytes(free: u64, total: u64, label_path: &std::path::Path) -> Check {
    let detail = format!(
        "{} free of {} ({}% free) on the filesystem holding {}",
        human_bytes(free),
        human_bytes(total),
        free_pct(free, total),
        label_path.display(),
    );
    match disk_level(free, total) {
        CheckLevel::Ok => Check::ok("disk-space", detail),
        CheckLevel::Warn => Check::warn("disk-space", detail, Some(DISK_FIX)),
        CheckLevel::Fail => Check::fail("disk-space", detail, Some(DISK_FIX)),
    }
}

/// The stat + classify half of [`disk_space_check`], split out so the
/// path resolution above stays readable. `label_path` is what the detail
/// line names (the data dir), `stat_path` is what we actually statvfs.
fn disk_check_at(stat_path: &std::path::Path, label_path: &std::path::Path) -> Check {
    match crate::disk::statvfs_bytes(stat_path) {
        Ok((free, total)) => disk_check_bytes(free, total, label_path),
        Err(e) => Check::warn(
            "disk-space",
            format!("could not stat {}: {e}", stat_path.display()),
            Some("free space is unknown — check the path exists and is readable"),
        ),
    }
}

/// Render the collected `doctor` checks and pick the exit code.
fn finalise_doctor(checks: &[Check], as_json: bool, palette: Palette) -> RunOutput {
    let any_fail = checks.iter().any(|c| c.level == CheckLevel::Fail);
    if as_json {
        let payload = serde_json::json!({
            "status": if any_fail { "fail" } else { "ok" },
            "checks": checks.iter().map(Check::to_json).collect::<Vec<_>>(),
        });
        let mut out = render_json_pretty(&payload);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return if any_fail {
            RunOutput::failure(out)
        } else {
            RunOutput::success(out)
        };
    }
    let mut out = String::new();
    for c in checks {
        // Style only the level tag: the padded plain tag keeps column
        // alignment independent of the ANSI escape bytes.
        let tag = match c.level {
            CheckLevel::Ok => palette.ok(c.level.tag()),
            CheckLevel::Warn => palette.warn(c.level.tag()),
            CheckLevel::Fail => palette.fail(c.level.tag()),
        };
        out.push_str(&format!("[{tag}] {:<18} {}\n", c.name, c.detail));
        if c.level != CheckLevel::Ok {
            if let Some(fix) = &c.fix {
                out.push_str(&format!("       {}\n", palette.fix(&format!("fix: {fix}"))));
            }
        }
    }
    if any_fail {
        out.push_str("\nat least one check is in FAIL state; see the `fix:` lines above\n");
        RunOutput::failure(out)
    } else if checks.iter().any(|c| c.level == CheckLevel::Warn) {
        out.push_str("\ninstall is reachable but has warnings; see the `fix:` lines above\n");
        RunOutput::success(out)
    } else {
        out.push_str("\nall checks passed; install is ready for `cclaw chat`\n");
        RunOutput::success(out)
    }
}

// ===========================================================================
// M21 O1 — doctor checks for the Wave-1 recovery matrix.
//
// Each `*_check` below is a pure classifier over already-fetched inputs (a
// probe result, a JSON response, or a resolved path) so both the healthy and
// failing branches are unit-testable without a live host — mirroring the
// `disk_check_bytes` split above. The only impure pieces are `probe_runtime`
// and `doctor_data_root`; both carry a `#[cfg(test)]` override so the
// full-flow doctor tests stay deterministic regardless of the test box's
// Docker daemon or filesystem.
// ===========================================================================

/// Outcome of probing the container runtime (Docker) for reachability.
#[derive(Debug, Clone)]
enum RuntimeProbe {
    /// The runtime endpoint accepted a connection; the string labels it
    /// (socket path or TCP address) for the detail line.
    Reachable(String),
    /// The endpoint could not be reached: `(endpoint, error)`.
    Unreachable(String, String),
}

/// Build the `container-runtime` row from a probe result. A host that can't
/// reach Docker can spawn no session containers at all, so an unreachable
/// runtime is a FAIL with the daemon-check command.
fn runtime_check_from(probe: &RuntimeProbe) -> Check {
    match probe {
        RuntimeProbe::Reachable(endpoint) => Check::ok(
            "container-runtime",
            format!("container runtime reachable ({endpoint})"),
        ),
        RuntimeProbe::Unreachable(endpoint, err) => Check::fail(
            "container-runtime",
            format!("cannot reach the container runtime at {endpoint}: {err}"),
            Some(
                "start the Docker daemon (`sudo systemctl start docker`, or launch Docker Desktop) and verify with `docker info`",
            ),
        ),
    }
}

/// Probe the Docker daemon the host talks to. Honors `DOCKER_HOST`
/// (`unix://` / `tcp://`), else the default `/var/run/docker.sock` — the
/// same endpoint `Docker::connect_with_socket_defaults` uses in
/// `copperclaw-container-rt`. A successful connect is the reachability
/// signal; we intentionally do not issue an API call, so a wedged-but-up
/// daemon still reads reachable (the host's own `version()` probe covers
/// that deeper case at spawn time).
#[cfg(not(test))]
fn probe_runtime() -> RuntimeProbe {
    match std::env::var("DOCKER_HOST").ok().filter(|s| !s.is_empty()) {
        Some(h) if h.starts_with("tcp://") => probe_runtime_tcp(h.trim_start_matches("tcp://")),
        Some(h) => probe_runtime_unix(h.trim_start_matches("unix://")),
        None => probe_runtime_unix("/var/run/docker.sock"),
    }
}

#[cfg(not(test))]
fn probe_runtime_unix(path: &str) -> RuntimeProbe {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => RuntimeProbe::Reachable(path.to_string()),
        Err(e) => RuntimeProbe::Unreachable(path.to_string(), e.to_string()),
    }
}

#[cfg(not(test))]
fn probe_runtime_tcp(addr: &str) -> RuntimeProbe {
    use std::net::ToSocketAddrs as _;
    let timeout = std::time::Duration::from_secs(2);
    match addr.to_socket_addrs().map(|mut it| it.next()) {
        Ok(Some(sa)) => match std::net::TcpStream::connect_timeout(&sa, timeout) {
            Ok(_) => RuntimeProbe::Reachable(addr.to_string()),
            Err(e) => RuntimeProbe::Unreachable(addr.to_string(), e.to_string()),
        },
        Ok(None) => RuntimeProbe::Unreachable(addr.to_string(), "no address resolved".to_string()),
        Err(e) => RuntimeProbe::Unreachable(addr.to_string(), e.to_string()),
    }
}

/// Runtime data root (`<install_root>/data`) for the filesystem-backed
/// doctor checks (heartbeat staleness, quarantine sidecars, central-DB
/// integrity). `None` when the install root can't be resolved — the
/// dependent rows are skipped rather than guessed.
#[cfg(not(test))]
fn doctor_data_root() -> Option<std::path::PathBuf> {
    resolve_install_root().map(|r| r.join("data"))
}

// Test seams for the two impure probes. Mirror the `DISK_OVERRIDE` pattern:
// thread-local, test-only, so the full-flow doctor tests are independent of
// the test box's Docker daemon and filesystem. `probe_runtime` defaults to
// reachable and `doctor_data_root` to `None` (filesystem checks skipped), so
// the pre-existing transport-seeded doctor tests only ever gain a single
// `container-runtime` OK row.
#[cfg(test)]
thread_local! {
    static RUNTIME_OVERRIDE: std::cell::RefCell<RuntimeProbe> =
        std::cell::RefCell::new(RuntimeProbe::Reachable("<test>".to_string()));
    static DATA_ROOT_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_runtime_override(probe: RuntimeProbe) {
    RUNTIME_OVERRIDE.with(|c| *c.borrow_mut() = probe);
}

#[cfg(test)]
fn probe_runtime() -> RuntimeProbe {
    RUNTIME_OVERRIDE.with(|c| c.borrow().clone())
}

#[cfg(test)]
fn set_data_root_override(root: Option<std::path::PathBuf>) {
    DATA_ROOT_OVERRIDE.with(|c| *c.borrow_mut() = root);
}

#[cfg(test)]
fn doctor_data_root() -> Option<std::path::PathBuf> {
    DATA_ROOT_OVERRIDE.with(|c| c.borrow().clone())
}

/// Build the `host-loops` row from a `host.status` (S1 supervisor) response.
/// FAIL when the supervisor-wide degraded flag is set, or any supervised
/// loop is not alive / individually degraded — the silent-death surface this
/// program exists to close; OK otherwise.
fn host_loops_check(status: &serde_json::Value) -> Check {
    let degraded = status
        .get("degraded")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let loops = status
        .get("loops")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut broken: Vec<String> = Vec::new();
    for l in &loops {
        let name = l
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let alive = l
            .get("alive")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let loop_degraded = l
            .get("degraded")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if !alive {
            broken.push(format!("{name} (dead)"));
        } else if loop_degraded {
            broken.push(format!("{name} (degraded)"));
        }
    }
    if degraded || !broken.is_empty() {
        let detail = if broken.is_empty() {
            "host supervisor is in degraded mode".to_string()
        } else {
            format!("stalled background loop(s): {}", broken.join(", "))
        };
        Check::fail(
            "host-loops",
            detail,
            Some(
                "restart the host: `copperclaw stop && copperclaw start`; then check `copperclaw logs`",
            ),
        )
    } else {
        Check::ok(
            "host-loops",
            format!(
                "{} background loop(s) alive; host not degraded",
                loops.len()
            ),
        )
    }
}

/// Heartbeat staleness ceiling for the `stuck-sessions` row, in
/// milliseconds. Mirrors `copperclaw_host_sweep::HEARTBEAT_STALE_MS` (90s)
/// so doctor's notion of "stale" agrees with the sweep's recovery trigger.
/// cclaw does not depend on the sweep crate; kept in sync by this comment.
const DOCTOR_HEARTBEAT_STALE_MS: u64 = 90_000;

/// Build the `stuck-sessions` row. For each running session, stat
/// `<data_root>/sessions/<agent_group>/<session>/.heartbeat`; a heartbeat
/// present but older than [`DOCTOR_HEARTBEAT_STALE_MS`] means the runner has
/// stopped touching it — the wedged / crashed-runner case S1/S2 recover
/// from. A *missing* heartbeat is not counted: a just-spawned session may
/// not have written one yet, and doctor should not cry wolf on every cold
/// start.
fn stuck_sessions_check(
    data_root: &std::path::Path,
    running: &[serde_json::Value],
    now: std::time::SystemTime,
) -> Check {
    let stale_after = std::time::Duration::from_millis(DOCTOR_HEARTBEAT_STALE_MS);
    let mut stale: Vec<String> = Vec::new();
    for s in running {
        let (Some(ag), Some(id)) = (
            s.get("agent_group_id").and_then(serde_json::Value::as_str),
            s.get("id").and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        let heartbeat = data_root
            .join("sessions")
            .join(ag)
            .join(id)
            .join(".heartbeat");
        let Ok(mtime) = std::fs::metadata(&heartbeat).and_then(|m| m.modified()) else {
            continue; // missing / unreadable heartbeat: skip (see doc comment)
        };
        if now.duration_since(mtime).is_ok_and(|d| d > stale_after) {
            stale.push(id.to_string());
        }
    }
    if stale.is_empty() {
        Check::ok(
            "stuck-sessions",
            format!("{} running session(s); none heartbeat-stale", running.len()),
        )
    } else {
        Check::fail(
            "stuck-sessions",
            format!(
                "{} running session(s) with stale heartbeats (wedged/crashed runner): {}",
                stale.len(),
                stale.join(", ")
            ),
            Some(
                "reset the session: `cclaw sessions delete <session-id>` (or restart the group: `cclaw groups restart <group-id>`)",
            ),
        )
    }
}

/// Per-group provider-chain health, distilled from a
/// `groups.provider.status` response for the [`provider_chain_check`] row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainState {
    /// No fallback chain configured — the inert single-provider default
    /// (`FallbackChain::is_empty` in `copperclaw-providers`).
    NoChain,
    /// A chain is configured and at least one entry is currently usable.
    Healthy,
    /// A chain is configured but every entry is degraded and still cooling
    /// down: the group has no provider it can reach right now.
    AllUnhealthy,
}

/// Whether one health row reports a currently-usable provider: `healthy`, or
/// degraded but past its `cooldown_until` (the failover selector re-probes —
/// treats as healthy — once the cooldown elapses).
fn provider_row_usable(h: &serde_json::Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    let status = h
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("healthy");
    if status == "healthy" {
        return true;
    }
    match h.get("cooldown_until").and_then(serde_json::Value::as_str) {
        Some(ts) => chrono::DateTime::parse_from_rfc3339(ts)
            .is_ok_and(|d| d.with_timezone(&chrono::Utc) <= now),
        None => false,
    }
}

/// Reduce one `groups.provider.status` response to a [`ChainState`]. A chain
/// entry is usable when it has no health row (never failed) or at least one
/// usable key; the chain is [`ChainState::AllUnhealthy`] only when no entry
/// is usable.
fn group_chain_state(status: &serde_json::Value, now: chrono::DateTime<chrono::Utc>) -> ChainState {
    let Some(entries) = status
        .get("chain")
        .and_then(serde_json::Value::as_array)
        .filter(|e| !e.is_empty())
    else {
        return ChainState::NoChain;
    };
    let health = status
        .get("health")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let any_usable = entries.iter().any(|e| {
        let provider = e.get("provider").and_then(serde_json::Value::as_str);
        let model = e.get("model").and_then(serde_json::Value::as_str);
        let rows: Vec<&serde_json::Value> = health
            .iter()
            .filter(|h| {
                h.get("provider").and_then(serde_json::Value::as_str) == provider
                    && h.get("model").and_then(serde_json::Value::as_str) == model
            })
            .collect();
        rows.is_empty() || rows.iter().any(|h| provider_row_usable(h, now))
    });
    if any_usable {
        ChainState::Healthy
    } else {
        ChainState::AllUnhealthy
    }
}

/// Build the `provider-chain` row from per-group [`ChainState`]s, or `None`
/// when there are no group states (every status call errored). An empty
/// chain across all groups is a WARN (the inert default has no fallback for
/// a dead primary), never a FAIL; a group whose whole chain is unhealthy is
/// a FAIL.
fn provider_chain_check(states: &[(String, ChainState)]) -> Option<Check> {
    if states.is_empty() {
        return None;
    }
    let dead: Vec<&str> = states
        .iter()
        .filter(|(_, s)| *s == ChainState::AllUnhealthy)
        .map(|(id, _)| id.as_str())
        .collect();
    if !dead.is_empty() {
        return Some(Check::fail(
            "provider-chain",
            format!(
                "provider chain has no reachable provider for group(s): {}",
                dead.join(", ")
            ),
            Some(
                "inspect health + credentials: `cclaw groups provider status <group-id>`; verify the chain's API keys / endpoints",
            ),
        ));
    }
    let configured = states
        .iter()
        .filter(|(_, s)| *s == ChainState::Healthy)
        .count();
    if configured == 0 {
        return Some(Check::warn(
            "provider-chain",
            "no provider fallback chain configured (single-provider default; a dead primary has no fallback)",
            Some(
                "configure a fallback chain: `cclaw groups provider set-chain <group-id> --chain <json>`",
            ),
        ));
    }
    Some(Check::ok(
        "provider-chain",
        format!("{configured} group(s) with a healthy provider chain"),
    ))
}

/// Build the `dead-letter` row from a `dropped-messages.outbound-list`
/// response. Any undeliverable outbound backlog is a FAIL: these rows drain
/// only via an explicit replay (auto-replay is a standing rejection). Rows
/// whose failure was `no_adapter` (S5) are called out — they clear only once
/// the channel is (re)configured.
fn dead_letter_check(rows: &serde_json::Value) -> Check {
    let rows = rows.as_array().cloned().unwrap_or_default();
    let total = rows.len();
    if total == 0 {
        return Check::ok("dead-letter", "no undeliverable outbound messages");
    }
    let no_adapter = rows
        .iter()
        .filter(|r| {
            r.get("last_error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|e| e.contains("no_adapter"))
        })
        .count();
    let detail = if no_adapter > 0 {
        format!("{total} undeliverable outbound message(s) ({no_adapter} with no channel adapter)")
    } else {
        format!("{total} undeliverable outbound message(s)")
    };
    Check::fail(
        "dead-letter",
        detail,
        Some(
            "inspect with `cclaw dropped-messages outbound-list`, then replay once the channel is back: `cclaw dropped-messages replay <id>`",
        ),
    )
}

/// Quarantine sidecar suffix written by O2. Kept in sync with
/// `copperclaw_host_sweep::checks::integrity::QUARANTINE_SIDECAR_NAME`
/// (cclaw does not depend on the sweep crate). O2 writes the marker as a
/// SIBLING of the session directory —
/// `<data_root>/sessions/<agent_group>/<session_uuid>.quarantined` — not
/// inside it, because the session dir is bind-mounted read-write into the
/// untrusted agent's container as `/data` and a marker there could be
/// forged by the agent. Existence alone means the session is quarantined;
/// the body is additive-only JSON parsed defensively for the optional
/// `detail` field.
const QUARANTINE_SIDECAR_NAME: &str = ".quarantined";

/// Walk the session tree for O2 quarantine sidecars. Returns
/// `(session_uuid, detail)` per quarantined session. The markers are
/// sibling files (`<session_uuid>.quarantined`) alongside the session
/// directories in each agent-group dir — NOT files inside the session dirs
/// (those are the container-writable `/data` mounts). Best-effort: an
/// unreadable dir or sidecar is skipped, not fatal.
fn scan_quarantined_sessions(data_root: &std::path::Path) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let Ok(agents) = std::fs::read_dir(data_root.join("sessions")) else {
        return out;
    };
    for agent in agents.flatten() {
        let Ok(entries) = std::fs::read_dir(agent.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            // Sibling marker: `<session_uuid>.quarantined`. Plain session
            // directories (no suffix) are skipped by the strip.
            let Some(session_uuid) = name.strip_suffix(QUARANTINE_SIDECAR_NAME) else {
                continue;
            };
            if session_uuid.is_empty() {
                continue;
            }
            let detail = std::fs::read_to_string(entry.path()).ok().and_then(|body| {
                serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| {
                        v.get("detail")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
            });
            out.push((session_uuid.to_string(), detail));
        }
    }
    out
}

/// Read-only `PRAGMA quick_check` on the central DB. Returns `Some(detail)`
/// on corruption (or an open/query failure of an existing file), `None` when
/// the DB is healthy or simply absent (a fresh install). Opens read-only —
/// never creates or mutates the file. (Local re-implementation of O2's
/// `copperclaw_db::integrity::quick_check`, which cclaw does not depend on.)
fn central_db_integrity(path: &std::path::Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let conn = match rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) {
        Ok(c) => c,
        Err(e) => return Some(format!("open failed: {e}")),
    };
    let mut stmt = match conn.prepare("PRAGMA quick_check") {
        Ok(s) => s,
        Err(e) => return Some(format!("quick_check prepare failed: {e}")),
    };
    let rows = match stmt.query_map([], |r| r.get::<_, String>(0)) {
        Ok(it) => it,
        Err(e) => return Some(format!("quick_check query failed: {e}")),
    };
    let mut lines = Vec::new();
    for row in rows {
        match row {
            Ok(s) => lines.push(s),
            Err(e) => return Some(format!("quick_check row error: {e}")),
        }
    }
    if lines.len() == 1 && lines[0] == "ok" {
        None
    } else {
        Some(lines.join("; "))
    }
}

/// Build the `db-integrity` row. Scans the session tree for O2 quarantine
/// sidecars and `quick_check`s the central DB; any quarantined session or a
/// corrupt central DB is a FAIL. Purely local (filesystem + read-only
/// `SQLite`) — no transport.
fn db_integrity_check(data_root: &std::path::Path) -> Check {
    if let Some(detail) = central_db_integrity(&data_root.join("copperclaw.db")) {
        return Check::fail(
            "db-integrity",
            format!("central DB failed integrity check: {detail}"),
            Some(
                "stop the host and restore from a backup: `cclaw db restore <backup-path>` (see `cclaw db backup`)",
            ),
        );
    }
    let quarantined = scan_quarantined_sessions(data_root);
    if quarantined.is_empty() {
        return Check::ok(
            "db-integrity",
            "central DB healthy; no quarantined sessions",
        );
    }
    let sample: Vec<String> = quarantined
        .iter()
        .take(3)
        .map(|(id, detail)| match detail {
            Some(d) => format!("{id} ({d})"),
            None => id.clone(),
        })
        .collect();
    Check::fail(
        "db-integrity",
        format!(
            "{} session(s) quarantined for DB corruption: {}",
            quarantined.len(),
            sample.join(", ")
        ),
        Some(
            "reset each quarantined session: `cclaw sessions delete <session-id>` (back up `<data>/sessions/<ag>/<session>/` first)",
        ),
    )
}

/// `cclaw completions <shell>` — render the clap-generated completion
/// script to stdout. No transport call; pure client-side.
fn run_completions(args: &serde_json::Value) -> RunOutput {
    use clap::CommandFactory as _;
    let Some(shell_str) = args.get("shell").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure("completions: missing shell\n".to_string());
    };
    let Ok(shell) = shell_str.parse::<clap_complete::Shell>() else {
        return RunOutput::failure(format!("completions: unknown shell {shell_str}\n"));
    };
    let mut cmd = Cli::command();
    let mut buf: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut cmd, "cclaw", &mut buf);
    let text = String::from_utf8(buf)
        .unwrap_or_else(|e| format!("completions: invalid utf8 from generator: {e}\n"));
    RunOutput::success(text)
}

/// `cclaw status` — gather a one-shot install overview by hitting four
/// list endpoints in parallel-ish (sequential round-trips, since the
/// transport doesn't support batching). Renders a digest of counts
/// plus a small table per resource so the user can immediately see
/// what's wired up.
async fn run_status<T>(transport: &T, caller: Caller, as_json: bool, palette: Palette) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let groups = match transport
        .call("groups.list", serde_json::json!({}), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("groups.list", &e)),
    };
    let mgs = match transport
        .call(
            "messaging-groups.list",
            serde_json::json!({}),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("messaging-groups.list", &e)),
    };
    let wirings = match transport
        .call("wirings.list", serde_json::json!({}), caller.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("wirings.list", &e)),
    };
    let sessions = match transport
        .call(
            "sessions.list",
            serde_json::json!({"status": "active"}),
            caller,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("sessions.list", &e)),
    };

    if as_json {
        let summary = serde_json::json!({
            "agent_groups": groups,
            "messaging_groups": mgs,
            "wirings": wirings,
            "active_sessions": sessions,
        });
        let mut out = render_json_pretty(&summary);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return RunOutput::success(out);
    }

    let mut out = String::new();
    out.push_str(&format!("agent groups:      {}\n", array_len(&groups)));
    out.push_str(&format!("messaging groups:  {}\n", array_len(&mgs)));
    out.push_str(&format!("wirings:           {}\n", array_len(&wirings)));
    out.push_str(&format!("active sessions:   {}\n", array_len(&sessions)));
    out.push('\n');
    if array_len(&groups) > 0 {
        out.push_str("agent groups\n");
        out.push_str(&render_with(&groups, palette));
        out.push_str("\n\n");
    }
    if array_len(&mgs) > 0 {
        out.push_str("messaging groups\n");
        out.push_str(&render_with(&mgs, palette));
        out.push_str("\n\n");
    }
    if array_len(&wirings) > 0 {
        out.push_str("wirings\n");
        out.push_str(&render_with(&wirings, palette));
        out.push_str("\n\n");
    }
    if array_len(&sessions) > 0 {
        out.push_str("active sessions\n");
        out.push_str(&render_with(&sessions, palette));
        out.push_str("\n\n");
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    RunOutput::success(out)
}

fn array_len(v: &serde_json::Value) -> usize {
    v.as_array().map_or(0, Vec::len)
}

async fn run_quickstart_cli<T>(
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
    as_json: bool,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let Some(name) = args.get("name").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure("quickstart.cli: missing name\n".to_string());
    };
    let folder = args
        .get("folder")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(name)
        .to_string();
    let pattern = args
        .get("pattern")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(".*")
        .to_string();
    let provider = args.get("provider").and_then(serde_json::Value::as_str);

    // 1) Create the agent group.
    let mut group_args = serde_json::Map::new();
    group_args.insert("folder".into(), folder.into());
    group_args.insert("name".into(), name.into());
    if let Some(p) = provider {
        group_args.insert("provider".into(), p.into());
    }
    let group = match transport
        .call(
            "groups.create",
            serde_json::Value::Object(group_args),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("groups.create", &e)),
    };
    let Some(ag_id) = group.get("id").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure("quickstart.cli: groups.create returned no id\n".to_string());
    };

    // 2) Create a messaging group bound to the cli/stdin channel.
    let mg = match transport
        .call(
            "messaging-groups.create",
            serde_json::json!({
                "channel_type": "cli",
                "platform_id": "stdin",
                "name": name,
                "is_group": false,
            }),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("messaging-groups.create", &e)),
    };
    let Some(mg_id) = mg.get("id").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure(
            "quickstart.cli: messaging-groups.create returned no id\n".to_string(),
        );
    };

    // 3) Wire them with a pattern-match engage mode.
    let wiring = match transport
        .call(
            "wirings.create",
            serde_json::json!({
                "agent_group_id": ag_id,
                "messaging_group_id": mg_id,
                "engage": "pattern",
                "pattern": pattern,
            }),
            caller,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format_step_error("wirings.create", &e)),
    };

    let summary = serde_json::json!({
        "agent_group": group,
        "messaging_group": mg,
        "wiring": wiring,
    });
    let text = if as_json {
        render_json_pretty(&summary)
    } else {
        format!(
            "agent group {name} ({ag}) is now wired to cli/stdin via messaging group {mg} (wiring {w}).\n",
            name = name,
            ag = ag_id,
            mg = mg_id,
            w = wiring
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?"),
        )
    };
    let mut out = text;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    RunOutput::success(out)
}

fn format_step_error(step: &str, e: &ClientError) -> String {
    match e {
        ClientError::Remote(p) => {
            format!(
                "quickstart.cli: {step} failed: {} ({})\n",
                p.message, p.code
            )
        }
        other => format!("quickstart.cli: {step} failed: {other}\n"),
    }
}
// ---------------------------------------------------------------------------
// `cclaw` (no args) — operator dashboard.
// ---------------------------------------------------------------------------

/// Treat a [`ClientError::Io`] with `NotFound` / `ConnectionRefused` as
/// "the host is not running" so the dashboard can print a friendly
/// pointer to `copperclaw start` instead of a raw I/O error.
fn host_unreachable(e: &ClientError) -> bool {
    if let ClientError::Io(err) = e {
        matches!(
            err.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::PermissionDenied
        )
    } else {
        false
    }
}

/// `cclaw` (no subcommand) — single-screen operator dashboard.
///
/// Fans out to the existing read-only handlers in parallel via
/// [`tokio::join`] so the wall time is bounded by the slowest call,
/// not the sum. The composite is pure client-side; no new socket
/// commands are introduced.
async fn run_dashboard<T>(
    transport: &T,
    caller: Caller,
    as_json: bool,
    palette: Palette,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let (groups, wirings, sessions, audit, dropped, usage) = tokio::join!(
        transport.call("groups.list", serde_json::json!({}), caller.clone()),
        transport.call("wirings.list", serde_json::json!({}), caller.clone()),
        transport.call(
            "sessions.list",
            serde_json::json!({"status": "active"}),
            caller.clone(),
        ),
        transport.call(
            "audit.list",
            serde_json::json!({"since": "1h", "limit": 50}),
            caller.clone(),
        ),
        transport.call(
            "dropped-messages.list",
            serde_json::json!({"since": "1h"}),
            caller.clone(),
        ),
        transport.call("usage.rollup", serde_json::json!({"since": "24h"}), caller),
    );

    // If the very first call failed because the socket is missing,
    // surface a friendly "host not running" message and exit non-zero
    // so scripts can detect it.
    if let Err(e) = &groups {
        if host_unreachable(e) {
            return RunOutput::failure(
                "host not running. Run `copperclaw start` to start it. \
                 Or `cclaw doctor` to diagnose.\n"
                    .to_string(),
            );
        }
    }

    // For the remaining sections, surface a remote error if the *first*
    // call failed for non-IO reasons; otherwise treat per-section errors
    // as "section unavailable" so a partial host (e.g. running but with
    // a degraded audit table) still renders something useful.
    let groups = match groups {
        Ok(v) => v,
        Err(e) => return RunOutput::failure(format!("dashboard: groups.list failed: {e}\n")),
    };
    let wirings = wirings.unwrap_or_else(|_| serde_json::json!([]));
    let sessions = sessions.unwrap_or_else(|_| serde_json::json!([]));
    let audit = audit.unwrap_or_else(|_| serde_json::json!([]));
    let dropped = dropped.unwrap_or_else(|_| serde_json::json!([]));
    let usage = usage.unwrap_or_else(|_| serde_json::json!([]));

    let install_root = resolve_install_root().map_or_else(
        || "(unknown)".to_string(),
        |p| p.to_string_lossy().into_owned(),
    );
    let suggestions = dashboard_suggestions(&groups, &audit, &dropped, &sessions);

    if as_json {
        let payload = serde_json::json!({
            "install_root": install_root,
            "agent_groups": groups,
            "wirings": wirings,
            "active_sessions": sessions,
            "recent_activity": {
                "audit": audit,
                "dropped": dropped,
                "usage": usage,
            },
            "suggestions": suggestions,
        });
        let mut out = render_json_pretty(&payload);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return RunOutput::success(out);
    }

    let text = render_dashboard_text(
        &install_root,
        &groups,
        &wirings,
        &sessions,
        &audit,
        &dropped,
        &usage,
        &suggestions,
        palette,
    );
    RunOutput::success(text)
}

/// Heuristic next-step picker. Capped at three suggestions; deduped
/// in input order.
fn dashboard_suggestions(
    groups: &serde_json::Value,
    audit: &serde_json::Value,
    dropped: &serde_json::Value,
    sessions: &serde_json::Value,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let group_count = array_len(groups);
    let active_audit = array_len(audit);
    let drop_count = array_len(dropped);
    let active_sessions = array_len(sessions);

    if group_count == 0 {
        out.push("cclaw quickstart cli --name first   # create your first agent group".into());
    }
    if drop_count > 0 {
        out.push("cclaw dropped-messages list --since 1h   # investigate dropped traffic".into());
    }
    if group_count >= 1 && active_audit == 0 && active_sessions == 0 {
        out.push(
            "cclaw chat                          # open a REPL against the cli channel".into(),
        );
    }
    // Always finish with the diagnostic / overview pointer so users
    // know where to go for more detail.
    if out.len() < 3 {
        out.push("cclaw status                        # full wiring digest".into());
    }
    if out.len() < 3 {
        out.push("cclaw health                        # operator health probe".into());
    }
    out.truncate(3);
    out
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)] // One line per dashboard section; splitting hurts readability.
fn render_dashboard_text(
    install_root: &str,
    groups: &serde_json::Value,
    wirings: &serde_json::Value,
    sessions: &serde_json::Value,
    audit: &serde_json::Value,
    dropped: &serde_json::Value,
    usage: &serde_json::Value,
    suggestions: &[String],
    palette: Palette,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{}\n\n",
        palette.header(&format!("copperclaw at {install_root}"))
    ));

    out.push_str(&format!(
        "{}\n",
        palette.header(&format!("agent groups ({})", array_len(groups)))
    ));
    if let Some(items) = groups.as_array() {
        if items.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for g in items {
                let id = json_str(g, "id");
                let name = json_str(g, "name");
                let provider = json_str(g, "agent_provider");
                let provider = if provider.is_empty() { "—" } else { provider };
                out.push_str(&format!("  {id:24}  {name:24}  provider={provider}\n",));
            }
        }
    }
    out.push('\n');

    out.push_str(&format!(
        "{}\n",
        palette.header(&format!("wirings ({})", array_len(wirings)))
    ));
    if let Some(items) = wirings.as_array() {
        if items.is_empty() {
            out.push_str("  (none)\n");
        } else {
            for w in items {
                let id = json_str(w, "id");
                let mg = json_str(w, "messaging_group_id");
                let ag = json_str(w, "agent_group_id");
                let engage = json_str(w, "engage");
                out.push_str(&format!("  {id:24}  mg={mg}  ag={ag}  engage={engage}\n",));
            }
        }
    }
    out.push('\n');

    out.push_str(&format!(
        "{}\n",
        palette.header(&format!("active sessions ({})", array_len(sessions)))
    ));
    if array_len(sessions) == 0 {
        out.push_str("  (none)\n");
    } else if let Some(items) = sessions.as_array() {
        for s in items {
            let id = json_str(s, "id");
            let status = json_str(s, "container_status");
            out.push_str(&format!("  {id:36}  {status}\n"));
        }
    }
    out.push('\n');

    let mutations = array_len(audit);
    let errors = audit.as_array().map_or(0, |arr| {
        arr.iter()
            .filter(|row| row.get("error").is_some_and(|v| !v.is_null()))
            .count()
    });
    let outbound_drops = array_len(dropped);
    out.push_str(&format!(
        "{}\n",
        palette.header("recent activity (last 1h)")
    ));
    out.push_str(&format!(
        "  audit:    {mutations} mutations, {errors} errors\n",
    ));
    out.push_str(&format!("  dropped:  {outbound_drops} messages\n"));
    if let Some(rows) = usage.as_array() {
        if rows.is_empty() {
            out.push_str("  budget:   (no token usage in last 24h)\n");
        } else {
            for r in rows {
                let ag = json_str(r, "agent_group_id");
                let total = r
                    .get("total_tokens")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                out.push_str(&format!("  budget:   {ag} {total} tokens (24h)\n",));
            }
        }
    }
    out.push('\n');

    out.push_str(&format!("{}\n", palette.header("suggested next:")));
    for s in suggestions {
        out.push_str(&format!("  {s}\n"));
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn json_str<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
}

// ---------------------------------------------------------------------------
// `cclaw groups config edit <id>` — open config in $EDITOR, diff, update.
// ---------------------------------------------------------------------------

/// Container-config fields that are safe to round-trip through
/// `groups.config.update`. Everything else is rendered as a comment in
/// the TOML and silently ignored if edited.
const EDITABLE_SCALAR_FIELDS: &[&str] = &[
    "provider",
    "model",
    "image_tag",
    "assistant_name",
    "max_messages_per_prompt",
    "tool_profile",
    // M17 session-preview proxy: master switch + bind interface. The host
    // validates both (`preview_enabled` bool; `preview_bind` IP-or-null).
    "preview_enabled",
    "preview_bind",
];

/// Fields the host returns but does not accept on update. They are
/// stripped from the editable region and re-rendered as `# read-only`
/// comments so the operator has context without being able to corrupt
/// them.
const READ_ONLY_FIELDS: &[&str] = &["agent_group_id", "updated_at"];

/// Run the EDITOR-driven edit-and-update workflow for `groups.config`.
#[allow(clippy::too_many_lines)]
async fn run_groups_config_edit<T>(
    args: &serde_json::Value,
    transport: &T,
    caller: Caller,
) -> RunOutput
where
    T: CallTransport + ?Sized,
{
    let Some(id) = args.get("id").and_then(serde_json::Value::as_str) else {
        return RunOutput::failure("groups.config.edit: missing id\n".to_string());
    };
    let dry_run = args
        .get("dry_run")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    // Tests may inject an EDITOR override via the args payload to avoid
    // mutating the process-global env var; the production path reads
    // EDITOR/VISUAL/vi in order.
    let editor = args
        .get("editor_override")
        .and_then(serde_json::Value::as_str)
        .map_or_else(
            || {
                std::env::var("EDITOR")
                    .or_else(|_| std::env::var("VISUAL"))
                    .unwrap_or_else(|_| "vi".to_string())
            },
            str::to_owned,
        );

    let current = match transport
        .call(
            "groups.config.get",
            serde_json::json!({"id": id}),
            caller.clone(),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            return RunOutput::failure(format!(
                "groups.config.edit: groups.config.get failed: {e}\n"
            ));
        }
    };
    let current_obj = match current.as_object() {
        Some(o) => o.clone(),
        None => {
            return RunOutput::failure(format!("groups.config.edit: no config exists for {id}\n"));
        }
    };

    let initial_toml = render_config_toml(&current_obj);

    // Persist the editable text to a temp file under the OS temp dir;
    // re-use the same path across retries so the operator keeps their
    // in-progress edits.
    let dir = std::env::temp_dir();
    let file_path = dir.join(format!("cclaw-config-{id}.toml"));
    if let Err(e) = tokio::fs::write(&file_path, &initial_toml).await {
        return RunOutput::failure(format!(
            "groups.config.edit: write {}: {e}\n",
            file_path.display()
        ));
    }

    // Retry loop: editor opens the temp file; on parse error we re-open
    // with an inline error and let the operator try again or abort.
    let edited_obj = loop {
        if let Err(e) = spawn_editor(&editor, &file_path).await {
            // EDITOR exited non-zero; treat as abort.
            let _ = tokio::fs::remove_file(&file_path).await;
            return RunOutput::failure(format!("groups.config.edit: editor failed: {e}\n"));
        }
        let bytes = match tokio::fs::read_to_string(&file_path).await {
            Ok(s) => s,
            Err(e) => {
                let _ = tokio::fs::remove_file(&file_path).await;
                return RunOutput::failure(format!(
                    "groups.config.edit: read {}: {e}\n",
                    file_path.display()
                ));
            }
        };
        if bytes == initial_toml {
            let _ = tokio::fs::remove_file(&file_path).await;
            return RunOutput::success("no changes\n".to_string());
        }
        match parse_config_toml(&bytes) {
            Ok(obj) => break obj,
            Err(err) => {
                // Re-prepend the error as a comment, then prompt.
                let annotated = annotate_with_parse_error(&bytes, &err);
                if let Err(e) = tokio::fs::write(&file_path, &annotated).await {
                    let _ = tokio::fs::remove_file(&file_path).await;
                    return RunOutput::failure(format!(
                        "groups.config.edit: write retry buffer: {e}\n"
                    ));
                }
                match prompt_retry_or_abort() {
                    RetryChoice::Retry => continue,
                    RetryChoice::Abort => {
                        let _ = tokio::fs::remove_file(&file_path).await;
                        return RunOutput::failure(
                            "groups.config.edit: aborted (config not updated)\n".to_string(),
                        );
                    }
                }
            }
        }
    };
    let _ = tokio::fs::remove_file(&file_path).await;

    let mut updates: Vec<(String, serde_json::Value)> = Vec::new();
    let mut mcp_servers_change: Option<serde_json::Value> = None;
    let mut packages_apt_change: Option<Vec<String>> = None;
    let mut packages_npm_change: Option<Vec<String>> = None;

    for key in EDITABLE_SCALAR_FIELDS {
        let old = current_obj
            .get(*key)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let new = edited_obj
            .get(*key)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if old != new {
            updates.push(((*key).to_string(), new));
        }
    }
    if let Some(new_mcp) = edited_obj.get("mcp_servers") {
        let old_mcp = current_obj
            .get("mcp_servers")
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        if &old_mcp != new_mcp {
            mcp_servers_change = Some(new_mcp.clone());
        }
    }
    if let Some(new_apt) = edited_obj.get("packages_apt").and_then(value_string_list) {
        let old_apt = current_obj
            .get("packages_apt")
            .and_then(value_string_list)
            .unwrap_or_default();
        if old_apt != new_apt {
            packages_apt_change = Some(new_apt);
        }
    }
    if let Some(new_npm) = edited_obj.get("packages_npm").and_then(value_string_list) {
        let old_npm = current_obj
            .get("packages_npm")
            .and_then(value_string_list)
            .unwrap_or_default();
        if old_npm != new_npm {
            packages_npm_change = Some(new_npm);
        }
    }

    let changed_field_names: Vec<String> = updates
        .iter()
        .map(|(k, _)| k.clone())
        .chain(mcp_servers_change.as_ref().map(|_| "mcp_servers".into()))
        .chain(packages_apt_change.as_ref().map(|_| "packages_apt".into()))
        .chain(packages_npm_change.as_ref().map(|_| "packages_npm".into()))
        .collect();

    if changed_field_names.is_empty() {
        return RunOutput::success("no changes\n".to_string());
    }

    if dry_run {
        let mut out = String::new();
        out.push_str("dry-run: would update the following fields:\n");
        for (k, v) in &updates {
            out.push_str(&format!("  {k} = {v}\n"));
        }
        if let Some(v) = &mcp_servers_change {
            out.push_str(&format!("  mcp_servers = {v}\n"));
        }
        if let Some(v) = &packages_apt_change {
            out.push_str(&format!("  packages_apt = {v:?}\n"));
        }
        if let Some(v) = &packages_npm_change {
            out.push_str(&format!("  packages_npm = {v:?}\n"));
        }
        return RunOutput::success(out);
    }

    // Commit scalar updates one at a time (the existing `groups.config.update`
    // contract is one field per call). Stop on the first failure so we
    // don't half-update.
    for (key, value) in &updates {
        let res = transport
            .call(
                "groups.config.update",
                serde_json::json!({"id": id, "field": key, "value": value}),
                caller.clone(),
            )
            .await;
        if let Err(e) = res {
            return RunOutput::failure(format!(
                "groups.config.edit: groups.config.update {key} failed: {e}\n"
            ));
        }
    }

    if let Some(new_mcp) = mcp_servers_change {
        if let Err(e) =
            update_mcp_servers(transport, id, &current_obj, &new_mcp, caller.clone()).await
        {
            return RunOutput::failure(e);
        }
    }
    if let Some(new_apt) = packages_apt_change {
        let old_apt = current_obj
            .get("packages_apt")
            .and_then(value_string_list)
            .unwrap_or_default();
        if let Err(e) =
            update_packages(transport, id, "apt", &old_apt, &new_apt, caller.clone()).await
        {
            return RunOutput::failure(e);
        }
    }
    if let Some(new_npm) = packages_npm_change {
        let old_npm = current_obj
            .get("packages_npm")
            .and_then(value_string_list)
            .unwrap_or_default();
        if let Err(e) = update_packages(transport, id, "npm", &old_npm, &new_npm, caller).await {
            return RunOutput::failure(e);
        }
    }

    RunOutput::success(format!(
        "updated {} field{}: {}\n",
        changed_field_names.len(),
        if changed_field_names.len() == 1 {
            ""
        } else {
            "s"
        },
        changed_field_names.join(", "),
    ))
}

fn value_string_list(v: &serde_json::Value) -> Option<Vec<String>> {
    v.as_array().map(|arr| {
        arr.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    })
}

async fn update_mcp_servers<T>(
    transport: &T,
    id: &str,
    current_obj: &serde_json::Map<String, serde_json::Value>,
    new_mcp: &serde_json::Value,
    caller: Caller,
) -> Result<(), String>
where
    T: CallTransport + ?Sized,
{
    let empty = serde_json::Value::Object(serde_json::Map::new());
    let old_mcp = current_obj.get("mcp_servers").unwrap_or(&empty);
    let old_obj = old_mcp.as_object().cloned().unwrap_or_default();
    let new_obj = new_mcp.as_object().cloned().unwrap_or_default();
    // Remove servers no longer present.
    for name in old_obj.keys() {
        if !new_obj.contains_key(name) {
            if let Err(e) = transport
                .call(
                    "groups.config.remove-mcp-server",
                    serde_json::json!({"id": id, "name": name}),
                    caller.clone(),
                )
                .await
            {
                return Err(format!(
                    "groups.config.edit: remove-mcp-server {name}: {e}\n"
                ));
            }
        }
    }
    // Add or replace anything present in `new` that differs.
    for (name, server) in &new_obj {
        if old_obj.get(name) == Some(server) {
            continue;
        }
        if let Err(e) = transport
            .call(
                "groups.config.add-mcp-server",
                serde_json::json!({"id": id, "server": server}),
                caller.clone(),
            )
            .await
        {
            return Err(format!("groups.config.edit: add-mcp-server {name}: {e}\n"));
        }
    }
    Ok(())
}

async fn update_packages<T>(
    transport: &T,
    id: &str,
    kind: &str,
    old: &[String],
    new: &[String],
    caller: Caller,
) -> Result<(), String>
where
    T: CallTransport + ?Sized,
{
    for name in old {
        if !new.iter().any(|x| x == name) {
            if let Err(e) = transport
                .call(
                    "groups.config.remove-package",
                    serde_json::json!({"id": id, "kind": kind, "name": name}),
                    caller.clone(),
                )
                .await
            {
                return Err(format!(
                    "groups.config.edit: remove-package {kind} {name}: {e}\n"
                ));
            }
        }
    }
    for name in new {
        if !old.iter().any(|x| x == name) {
            if let Err(e) = transport
                .call(
                    "groups.config.add-package",
                    serde_json::json!({"id": id, "kind": kind, "name": name}),
                    caller.clone(),
                )
                .await
            {
                return Err(format!(
                    "groups.config.edit: add-package {kind} {name}: {e}\n"
                ));
            }
        }
    }
    Ok(())
}

/// Render the JSON object returned by `groups.config.get` as a TOML
/// document. Read-only fields appear as `# read-only` comments.
fn render_config_toml(obj: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut out = String::new();
    out.push_str("# cclaw groups config edit — TOML buffer\n");
    out.push_str("# Edit values, save, and close to apply. Re-open with --dry-run to preview.\n");
    out.push_str("# Read-only fields are shown for reference and ignored on save.\n\n");

    // Read-only fields first as comments.
    for key in READ_ONLY_FIELDS {
        if let Some(v) = obj.get(*key) {
            out.push_str(&format!("# read-only: {key} = {v}\n"));
        }
    }
    out.push('\n');

    // Editable scalar fields (provider, model, ...). Null values are
    // rendered as commented-out lines so the operator can uncomment to
    // set them.
    for key in EDITABLE_SCALAR_FIELDS {
        let v = obj.get(*key).cloned().unwrap_or(serde_json::Value::Null);
        out.push_str(&render_scalar_line(key, &v));
    }
    // Read-only display of the per-group coding-skills toggle so the
    // operator can see the current state when they `cclaw groups
    // config edit`. Toggled via `cclaw groups enable-coding <id>` /
    // `disable-coding <id>` — not through this buffer.
    let coding_enabled = obj
        .get("coding_enabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    out.push_str(&format!(
        "# read-only: coding_enabled = {coding_enabled}   # toggle via `cclaw groups enable-coding <id>` / `disable-coding <id>`\n"
    ));
    out.push('\n');

    // Packages — render as arrays of strings.
    if let Some(arr) = obj.get("packages_apt").and_then(value_string_list) {
        out.push_str(&format!("packages_apt = {}\n", toml_string_array(&arr)));
    }
    if let Some(arr) = obj.get("packages_npm").and_then(value_string_list) {
        out.push_str(&format!("packages_npm = {}\n", toml_string_array(&arr)));
    }
    out.push('\n');

    // mcp_servers — round-trip via toml::Value so nested JSON objects
    // are rendered as inline tables.
    let mcp = obj
        .get("mcp_servers")
        .cloned()
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
    if let Some(mcp_obj) = mcp.as_object() {
        if mcp_obj.is_empty() {
            out.push_str("[mcp_servers]\n");
        } else {
            for (name, server) in mcp_obj {
                out.push_str(&format!(
                    "[mcp_servers.{name}]\n{}\n",
                    json_value_as_toml_table_body(server),
                ));
            }
        }
    }

    out
}

fn render_scalar_line(key: &str, value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => format!("# {key} = \"\"   # currently null\n"),
        serde_json::Value::String(s) => format!("{key} = {}\n", toml_quote(s)),
        serde_json::Value::Bool(b) => format!("{key} = {b}\n"),
        serde_json::Value::Number(n) => format!("{key} = {n}\n"),
        other => format!("# {key} = {other}   # complex value, edit via socket\n"),
    }
}

fn toml_quote(s: &str) -> String {
    // Use the `toml` crate's quoting via `toml::Value::String`.
    toml::Value::String(s.to_string()).to_string()
}

fn toml_string_array(items: &[String]) -> String {
    let mut s = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&toml_quote(item));
    }
    s.push(']');
    s
}

/// Render a JSON object as the body of a TOML table (no surrounding
/// `[name]` header). Each key becomes `key = <toml-encoded-value>`.
fn json_value_as_toml_table_body(v: &serde_json::Value) -> String {
    let mut out = String::new();
    if let Some(obj) = v.as_object() {
        for (k, v) in obj {
            out.push_str(&format!("{k} = {}\n", json_to_toml_inline(v)));
        }
    }
    out
}

fn json_to_toml_inline(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => "\"\"".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => toml_quote(s),
        serde_json::Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(json_to_toml_inline).collect();
            format!("[{}]", parts.join(", "))
        }
        serde_json::Value::Object(obj) => {
            // Inline table.
            let parts: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{k} = {}", json_to_toml_inline(v)))
                .collect();
            format!("{{ {} }}", parts.join(", "))
        }
    }
}

/// Parse the post-edit TOML buffer back into a JSON object.
fn parse_config_toml(text: &str) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let table: toml::Value = toml::from_str(text).map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::to_value(&table).map_err(|e| e.to_string())?;
    json.as_object()
        .cloned()
        .ok_or_else(|| "TOML root must be a table".to_string())
}

fn annotate_with_parse_error(text: &str, err: &str) -> String {
    let mut out = String::new();
    out.push_str("# TOML parse error — edit and save to retry:\n");
    for line in err.lines() {
        out.push_str(&format!("#   {line}\n"));
    }
    out.push('\n');
    // Strip any previous error banner so retries don't accumulate.
    let mut in_banner = false;
    for line in text.lines() {
        if line.starts_with("# TOML parse error") {
            in_banner = true;
            continue;
        }
        if in_banner {
            if line.starts_with("#   ") || line.trim().is_empty() {
                continue;
            }
            in_banner = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[derive(Debug, Clone, Copy)]
enum RetryChoice {
    Retry,
    Abort,
}

/// Prompt on the controlling terminal. Tests should never reach this
/// path because they use `--dry-run` or never trigger a parse error.
fn prompt_retry_or_abort() -> RetryChoice {
    use std::io::{BufRead as _, Write as _};
    eprint!("groups.config.edit: parse error. (r)etry / (a)bort? ");
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return RetryChoice::Abort;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "r" | "retry" | "" => RetryChoice::Retry,
        _ => RetryChoice::Abort,
    }
}

/// Spawn `editor` on `path` and wait for it to exit.
///
/// Splits the editor string on ASCII whitespace so callers can pass
/// values like `EDITOR='code --wait'`. Non-zero exit codes are
/// reported as errors so the workflow aborts cleanly.
async fn spawn_editor(editor: &str, path: &std::path::Path) -> Result<(), String> {
    let mut parts = editor.split_whitespace();
    let Some(program) = parts.next() else {
        return Err("EDITOR is empty".into());
    };
    let extra: Vec<&str> = parts.collect();
    let status = tokio::process::Command::new(program)
        .args(&extra)
        .arg(path)
        .status()
        .await
        .map_err(|e| format!("spawn {program}: {e}"))?;
    if !status.success() {
        return Err(format!("{program} exited with {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_types::{AgentGroupId, SessionId};
    use serde_json::json;
    use std::sync::Mutex;

    struct StubTransport {
        result: Mutex<Option<Result<serde_json::Value, ClientError>>>,
        last_call: Mutex<Option<(String, serde_json::Value, Caller)>>,
    }

    impl StubTransport {
        fn ok(value: serde_json::Value) -> Self {
            Self {
                result: Mutex::new(Some(Ok(value))),
                last_call: Mutex::new(None),
            }
        }
        fn err(e: ClientError) -> Self {
            Self {
                result: Mutex::new(Some(Err(e))),
                last_call: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl CallTransport for StubTransport {
        async fn call(
            &self,
            command: &str,
            args: serde_json::Value,
            caller: Caller,
        ) -> Result<serde_json::Value, ClientError> {
            *self.last_call.lock().unwrap() = Some((command.to_string(), args, caller));
            self.result.lock().unwrap().take().unwrap()
        }
    }

    #[tokio::test]
    async fn run_cli_success_renders_table() {
        let t = StubTransport::ok(json!([{"id":"ag_1","name":"x"}]));
        let out = run_cli(["cclaw", "groups", "list"], &t).await;
        assert!(matches!(out.code, ExitCode { .. }));
        // ExitCode doesn't expose the raw u8 for comparison, but stdout
        // and absence of stderr confirm success.
        assert!(out.stderr.is_empty());
        assert!(out.stdout.contains("ID"));
        assert!(out.stdout.contains("ag_1"));
        let captured = t.last_call.lock().unwrap();
        let (cmd, _args, _caller) = captured.as_ref().unwrap();
        assert_eq!(cmd, "groups.list");
    }

    #[tokio::test]
    async fn run_cli_json_flag_emits_pretty_json() {
        let t = StubTransport::ok(json!([{"id":"x"}]));
        let out = run_cli(["cclaw", "--json", "groups", "list"], &t).await;
        assert!(out.stdout.contains("\"id\""));
        assert!(out.stdout.contains("[\n"));
    }

    #[tokio::test]
    async fn run_cli_remote_error_to_stderr() {
        let t = StubTransport::err(ClientError::Remote(ErrorPayload::new(
            "not-found",
            "no such id",
        )));
        let out = run_cli(["cclaw", "groups", "get", "x"], &t).await;
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("not-found"));
        assert!(out.stderr.contains("no such id"));
    }

    #[tokio::test]
    async fn run_cli_transport_error_to_stderr() {
        let t = StubTransport::err(ClientError::Timeout);
        let out = run_cli(["cclaw", "groups", "list"], &t).await;
        assert!(out.stderr.contains("timed out"));
    }

    #[tokio::test]
    async fn run_cli_parse_error_help_text_on_stderr() {
        let t = StubTransport::ok(json!({}));
        let out = run_cli(["cclaw", "no-such-command"], &t).await;
        assert!(!out.stderr.is_empty());
    }

    #[tokio::test]
    async fn run_cli_help_goes_to_stdout() {
        let t = StubTransport::ok(json!({}));
        let out = run_cli(["cclaw", "--help"], &t).await;
        assert!(!out.stdout.is_empty());
    }

    #[test]
    fn caller_from_raw_unset_is_host() {
        assert!(matches!(caller_from_raw(None).unwrap(), Caller::Host));
    }

    #[test]
    fn caller_from_raw_empty_is_host() {
        assert!(matches!(caller_from_raw(Some("")).unwrap(), Caller::Host));
        assert!(matches!(
            caller_from_raw(Some("   \t")).unwrap(),
            Caller::Host,
        ));
    }

    #[test]
    fn caller_from_raw_long_form() {
        let sid = SessionId::nil();
        let agid = AgentGroupId::nil();
        let json = format!(
            "{{\"session_id\":\"{}\",\"agent_group_id\":\"{}\"}}",
            sid.as_uuid(),
            agid.as_uuid(),
        );
        let c = caller_from_raw(Some(&json)).unwrap();
        if let Caller::Agent {
            session_id,
            agent_group_id,
            messaging_group_id,
        } = c
        {
            assert_eq!(session_id, sid);
            assert_eq!(agent_group_id, agid);
            assert!(messaging_group_id.is_none());
        } else {
            panic!("expected agent");
        }
    }

    #[test]
    fn caller_from_raw_short_form() {
        let sid = SessionId::nil();
        let agid = AgentGroupId::nil();
        let json = format!(
            "{{\"session\":\"{}\",\"agent_group\":\"{}\"}}",
            sid.as_uuid(),
            agid.as_uuid(),
        );
        let c = caller_from_raw(Some(&json)).unwrap();
        assert!(matches!(c, Caller::Agent { .. }));
    }

    #[test]
    fn caller_from_raw_invalid_json_errors() {
        let err = caller_from_raw(Some("garbage")).unwrap_err();
        assert!(matches!(err, RunError::BadAgentCaller(_)));
    }

    #[test]
    fn caller_from_env_uses_process_env() {
        // We don't mutate the env in tests (it's `unsafe` under edition
        // 2024). Instead, just confirm the no-env-var branch is `Host`.
        if std::env::var(AGENT_CALLER_ENV).is_err() {
            let c = caller_from_env().unwrap();
            assert!(matches!(c, Caller::Host));
        }
    }

    #[test]
    fn run_output_helpers() {
        let s = RunOutput::success("hi".into());
        assert!(s.stderr.is_empty());
        assert_eq!(s.stdout, "hi");
        let f = RunOutput::failure("err".into());
        assert!(f.stdout.is_empty());
        assert_eq!(f.stderr, "err");
    }

    #[test]
    fn run_error_display_includes_inner_messages() {
        let e = RunError::BadAgentCaller("bad".into());
        assert!(e.to_string().contains("bad"));
        let e = RunError::ParseCli("p".into());
        assert!(e.to_string().contains('p'));
        let inner = ClientError::Timeout;
        let e = RunError::Client(inner);
        assert!(e.to_string().contains("timed out"));
    }

    // ---- quickstart ------------------------------------------------------

    /// Stub that returns one queued response per call and records the
    /// `(command, args)` pairs in order.
    struct SequencedTransport {
        responses: Mutex<Vec<Result<serde_json::Value, ClientError>>>,
        calls: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl SequencedTransport {
        fn new(responses: Vec<Result<serde_json::Value, ClientError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl CallTransport for SequencedTransport {
        async fn call(
            &self,
            command: &str,
            args: serde_json::Value,
            _caller: Caller,
        ) -> Result<serde_json::Value, ClientError> {
            self.calls.lock().unwrap().push((command.to_string(), args));
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                return Err(ClientError::Timeout);
            }
            q.remove(0)
        }
    }

    #[tokio::test]
    async fn quickstart_cli_fans_out_three_calls() {
        let t = SequencedTransport::new(vec![
            Ok(json!({"id": "ag-1", "name": "demo", "folder": "demo"})),
            Ok(json!({"id": "mg-1", "channel_type": "cli", "platform_id": "stdin"})),
            Ok(json!({"id": "w-1"})),
        ]);
        let out = run_cli(["cclaw", "quickstart", "cli", "--name", "demo"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls[0].0, "groups.create");
        assert_eq!(calls[0].1["folder"], "demo");
        assert_eq!(calls[0].1["name"], "demo");
        assert_eq!(calls[1].0, "messaging-groups.create");
        assert_eq!(calls[1].1["channel_type"], "cli");
        assert_eq!(calls[1].1["platform_id"], "stdin");
        assert_eq!(calls[2].0, "wirings.create");
        assert_eq!(calls[2].1["agent_group_id"], "ag-1");
        assert_eq!(calls[2].1["messaging_group_id"], "mg-1");
        assert_eq!(calls[2].1["engage"], "pattern");
        assert_eq!(calls[2].1["pattern"], ".*");
        assert!(out.stdout.contains("ag-1"));
        assert!(out.stdout.contains("mg-1"));
        assert!(out.stdout.contains("w-1"));
    }

    #[tokio::test]
    async fn quickstart_cli_folder_override_propagates() {
        let t = SequencedTransport::new(vec![
            Ok(json!({"id": "ag-1"})),
            Ok(json!({"id": "mg-1"})),
            Ok(json!({"id": "w-1"})),
        ]);
        let _ = run_cli(
            [
                "cclaw",
                "quickstart",
                "cli",
                "--name",
                "demo",
                "--folder",
                "homedir",
                "--pattern",
                "^hi",
            ],
            &t,
        )
        .await;
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls[0].1["folder"], "homedir");
        assert_eq!(calls[2].1["pattern"], "^hi");
    }

    #[tokio::test]
    async fn completions_bash_renders_without_transport() {
        // Use a transport that would error if dialled — completions
        // must not hit it.
        let t = SequencedTransport::new(vec![]);
        let out = run_cli(["cclaw", "completions", "bash"], &t).await;
        assert!(out.stderr.is_empty());
        assert!(out.stdout.starts_with("_cclaw()"));
        assert!(t.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn completions_zsh_starts_with_compdef() {
        let t = SequencedTransport::new(vec![]);
        let out = run_cli(["cclaw", "completions", "zsh"], &t).await;
        assert!(out.stdout.starts_with("#compdef cclaw"));
    }

    #[tokio::test]
    async fn status_fans_out_four_list_calls() {
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1", "name": "demo"}])),
            Ok(json!([{"id": "mg-1", "channel_type": "cli"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
        ]);
        let out = run_cli(["cclaw", "status"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls[0].0, "groups.list");
        assert_eq!(calls[1].0, "messaging-groups.list");
        assert_eq!(calls[2].0, "wirings.list");
        assert_eq!(calls[3].0, "sessions.list");
        assert_eq!(calls[3].1["status"], "active");
        assert!(out.stdout.contains("agent groups:      1"));
        assert!(out.stdout.contains("messaging groups:  1"));
        assert!(out.stdout.contains("wirings:           1"));
        assert!(out.stdout.contains("active sessions:   0"));
    }

    #[tokio::test]
    async fn quickstart_cli_stops_on_first_error() {
        // First call fails — second/third should never run.
        let t = SequencedTransport::new(vec![Err(ClientError::Remote(ErrorPayload {
            code: "validation".into(),
            message: "folder required".into(),
            retryable: false,
            data: None,
        }))]);
        let out = run_cli(["cclaw", "quickstart", "cli", "--name", "demo"], &t).await;
        assert!(!out.stderr.is_empty());
        assert!(out.stderr.contains("groups.create"));
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
    }

    // ── cclaw doctor ───────────────────────────────────────────────

    #[test]
    fn disk_level_ok_when_plenty_free() {
        // 500 GiB free of 1 TiB: well above every threshold.
        let level = disk_level(500 * GIB, 1024 * GIB);
        assert_eq!(level, CheckLevel::Ok);
    }

    #[test]
    fn disk_level_warn_on_low_percent() {
        // 8% free of a large disk (>20 GiB free, so only the percent
        // rule can trip): 80 GiB free of 1 TiB → WARN, not FAIL.
        let level = disk_level(80 * GIB, 1000 * GIB);
        assert_eq!(level, CheckLevel::Warn);
    }

    #[test]
    fn disk_level_warn_on_low_absolute_even_when_percent_is_fine() {
        // 15 GiB free of a tiny 30 GiB disk: 50% free (percent rule OK)
        // but under the 20 GiB absolute WARN floor → WARN.
        let level = disk_level(15 * GIB, 30 * GIB);
        assert_eq!(level, CheckLevel::Warn);
    }

    #[test]
    fn disk_level_fail_on_critical_percent() {
        // 2% free of a large disk: 20 GiB free of 1000 GiB. Under 3%
        // (FAIL) even though 20 GiB is at the WARN absolute floor.
        let level = disk_level(20 * GIB, 1000 * GIB);
        assert_eq!(level, CheckLevel::Fail);
    }

    #[test]
    fn disk_level_fail_on_critical_absolute() {
        // 4 GiB free of a 100 GiB disk: 4% (above 3%) but under the
        // 5 GiB absolute FAIL floor → FAIL.
        let level = disk_level(4 * GIB, 100 * GIB);
        assert_eq!(level, CheckLevel::Fail);
    }

    #[test]
    fn disk_level_boundaries_are_inclusive_of_ok_at_the_edge() {
        // Exactly 10% free, well above the 20 GiB floor → not *below* the
        // threshold → OK. 200 GiB of 2000 GiB is exactly 10%.
        assert_eq!(disk_level(200 * GIB, 2000 * GIB), CheckLevel::Ok);
        // A hair under 10% → WARN.
        assert_eq!(disk_level(199 * GIB, 2000 * GIB), CheckLevel::Warn);
        // Exactly at the 20 GiB WARN floor with healthy pct → OK.
        assert_eq!(disk_level(20 * GIB, 40 * GIB), CheckLevel::Ok);
        // Just under 20 GiB free (healthy pct) → WARN via the absolute floor.
        assert_eq!(disk_level(19 * GIB, 40 * GIB), CheckLevel::Warn);
        // Exactly 3% free (not *below* 3%) but under the 5 GiB FAIL floor:
        // 3 GiB of 100 GiB → FAIL via the absolute floor.
        assert_eq!(disk_level(3 * GIB, 100 * GIB), CheckLevel::Fail);
    }

    #[test]
    fn disk_level_handles_zero_total_without_panicking() {
        // Degenerate stat (total 0): percent rule short-circuits to
        // false, free 0 < 5 GiB → FAIL. Must not divide-by-zero.
        assert_eq!(disk_level(0, 0), CheckLevel::Fail);
    }

    #[test]
    fn disk_check_degrades_to_warn_on_stat_failure() {
        // A path that cannot be statvfs'd must yield a WARN row (never a
        // panic), so a stat error can't take down the rest of doctor.
        let missing = std::path::Path::new("/copperclaw/definitely/not/here");
        let check = disk_check_at(missing, missing);
        assert_eq!(check.level, CheckLevel::Warn);
        assert!(check.detail.contains("could not stat"));
        assert!(check.fix.is_some());
    }

    #[test]
    fn human_bytes_renders_binary_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(20 * GIB), "20.0 GiB");
        assert_eq!(human_bytes(1536 * 1024 * 1024), "1.5 GiB");
    }

    #[test]
    fn free_pct_is_floored_and_zero_safe() {
        assert_eq!(free_pct(50 * GIB, 100 * GIB), 50);
        assert_eq!(free_pct(1, 3), 33); // 33.3% floored
        assert_eq!(free_pct(0, 0), 0); // zero-total is safe, not a panic
    }

    #[tokio::test]
    async fn doctor_reports_disk_space_row_with_healthy_default() {
        // No override → healthy synthetic disk → OK row present, no FAIL.
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        assert!(out.stdout.contains("disk-space"));
        assert!(!out.stdout.contains("[FAIL"));
    }

    #[tokio::test]
    async fn doctor_disk_space_fail_surfaces_fix_line() {
        // Critically low disk → FAIL row + `fix:` hint; whole report moves
        // to stderr (finalise_doctor uses failure() when any row FAILs).
        set_disk_override(GIB, 100 * GIB);
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        assert!(out.stderr.contains("FAIL"));
        assert!(out.stderr.contains("disk-space"));
        assert!(out.stderr.contains("fix:"));
        assert!(out.stderr.contains("reclaim space"));
    }

    #[tokio::test]
    async fn doctor_disk_space_warn_stays_on_stdout() {
        // Low-but-not-critical disk → WARN row (success exit, stdout).
        set_disk_override(60 * GIB, 1000 * GIB); // 6% free, >5 GiB
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        assert!(
            out.stderr.is_empty(),
            "WARN must not fail: {:?}",
            out.stderr
        );
        assert!(out.stdout.contains("WARN"));
        assert!(out.stdout.contains("disk-space"));
    }

    #[tokio::test]
    async fn doctor_json_carries_disk_space_row() {
        // --json output must include the disk-space check consistently
        // with other rows (name/level/detail/fix keys).
        set_disk_override(GIB, 100 * GIB);
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["cclaw", "--json", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        let v: serde_json::Value = serde_json::from_str(&combined).expect("valid json");
        assert_eq!(v["status"], "fail");
        let disk = v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "disk-space")
            .expect("disk-space row present in json");
        assert_eq!(disk["level"], "fail");
        assert!(disk["detail"].is_string());
        assert!(disk["fix"].is_string());
    }

    #[tokio::test]
    async fn doctor_socket_unreachable_fails_with_fix() {
        // First call (groups.list) errors → doctor bails out with a FAIL row
        // pointing at `copperclaw run`.
        let t = SequencedTransport::new(vec![Err(ClientError::Timeout)]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        assert!(
            out.stdout.is_empty(),
            "doctor must use stderr for fail: stdout={:?}",
            out.stdout
        );
        assert!(out.stderr.contains("FAIL"));
        assert!(out.stderr.contains("host-reachable"));
        assert!(out.stderr.contains("copperclaw run"));
        // It should *not* keep going and hit downstream endpoints once
        // the socket is gone.
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
    }

    #[tokio::test]
    async fn doctor_empty_install_warns_about_groups_and_wirings() {
        // groups=[], wirings=[], active sessions=[], audit=[], dropped=[]
        let t = SequencedTransport::new(vec![
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(
            combined.contains("FAIL"),
            "must include FAIL for missing agent group"
        );
        assert!(combined.contains("agent-group"));
        assert!(combined.contains("cclaw quickstart cli"));
        // Wirings is gated on group existence; with zero groups the
        // wiring row will show WARN (no wirings). Either way, the
        // remediation hint mentions quickstart.
    }

    #[tokio::test]
    async fn doctor_happy_path_has_no_fails() {
        // groups=1, wirings=1, sessions=[], audit=[], dropped=[]
        // We deliberately do NOT assert on the ANTHROPIC_API_KEY check
        // result — the cclaw process inherits the test runner's env,
        // which may or may not have the key set, and doctor's purpose
        // is to surface that mismatch as a WARN rather than treating
        // it as a hard failure.
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        // Happy-path doctor never lands in stderr because no checks fail.
        assert!(
            out.stderr.is_empty(),
            "expected no stderr on happy doctor; got: {:?}",
            out.stderr,
        );
        assert!(out.stdout.contains("OK"));
        assert!(out.stdout.contains("host-reachable"));
        assert!(out.stdout.contains("agent-group"));
        assert!(out.stdout.contains("wiring"));
        // No FAIL rows, and the trailer is one of the two non-failure
        // forms ("all checks passed" or "install is reachable but has
        // warnings"), never the failure trailer.
        assert!(!out.stdout.contains("FAIL"));
        assert!(
            out.stdout.contains("all checks passed")
                || out.stdout.contains("install is reachable but has warnings"),
            "unexpected trailer: {:?}",
            out.stdout,
        );
    }

    #[test]
    fn budgets_check_ok_when_no_breach() {
        let c = budgets_check(&json!([
            {"agent_group_id": "ag-1", "over_daily_token_cap": false, "over_daily_cost_cap": false},
        ]));
        assert!(matches!(c.level, CheckLevel::Ok));
        assert!(c.detail.contains("no agent group is over"));
    }

    #[test]
    fn budgets_check_warns_on_breach_with_fix_hint() {
        let c = budgets_check(&json!([
            {"agent_group_id": "ag-cost", "over_daily_token_cap": false, "over_daily_cost_cap": true},
            {"agent_group_id": "ag-both", "over_daily_token_cap": true, "over_daily_cost_cap": true},
            {"agent_group_id": "ag-fine", "over_daily_token_cap": false, "over_daily_cost_cap": false},
        ]));
        assert!(matches!(c.level, CheckLevel::Warn));
        assert!(
            c.detail.contains("2 group(s) over a daily cap"),
            "{}",
            c.detail
        );
        assert!(c.detail.contains("ag-cost (cost cap)"), "{}", c.detail);
        assert!(
            c.detail.contains("ag-both (token+cost caps)"),
            "{}",
            c.detail
        );
        assert!(!c.detail.contains("ag-fine"), "{}", c.detail);
        let fix = c.fix.as_deref().unwrap();
        assert!(fix.contains("--daily-cost"), "{fix}");
    }

    #[test]
    fn budgets_check_ok_on_legacy_payload_without_flags() {
        // An older host without the breach flags must read as OK, not
        // guessed-at breach.
        let c = budgets_check(&json!([{"agent_group_id": "ag-old", "daily_token_cap": 5}]));
        assert!(matches!(c.level, CheckLevel::Ok));
    }

    #[tokio::test]
    async fn doctor_reports_egress_allow_all_as_ok() {
        // 6th seeded response is the egress.status call. allow-all → OK row.
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!({
                "mode": "allow-all",
                "model_endpoint": "api.anthropic.com:443",
                "groups": [{"agent_group_id": "ag-1", "effective_allow": ["api.anthropic.com:443"]}],
            })),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(combined.contains("egress"));
        assert!(combined.contains("allow-all"));
        assert!(combined.contains("api.anthropic.com:443"));
        assert!(!out.stdout.contains("FAIL"));
    }

    #[tokio::test]
    async fn doctor_reports_egress_deny_default_as_warn() {
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!({
                "mode": "deny-default",
                "model_endpoint": "api.anthropic.com:443",
                "dns_filter": true,
                "nft_status": "available",
                "groups": [{"agent_group_id": "ag-1", "effective_allow": ["api.anthropic.com:443"]}],
            })),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(combined.contains("WARN"));
        assert!(combined.contains("egress"));
        assert!(combined.contains("deny-default"));
        // Phase 0a v2: the DNS-filter layer (enforced) must be surfaced.
        assert!(
            combined.contains("DNS filtering ON"),
            "doctor must report DNS filtering status: {combined}"
        );
        // The nftables netns-apply deferral caveat must be surfaced, not hidden.
        assert!(combined.contains("deferred"));
    }

    #[tokio::test]
    async fn doctor_reports_egress_deny_default_nft_tool_missing() {
        // When `nft` isn't on the host PATH, doctor must say the L3/L4 ruleset
        // was constructed but NOT applied — never claim enforcement we lack.
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!({
                "mode": "deny-default",
                "model_endpoint": "api.anthropic.com:443",
                "dns_filter": true,
                "nft_status": "tool-missing",
                "groups": [{"agent_group_id": "ag-1", "effective_allow": ["api.anthropic.com:443"]}],
            })),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(combined.contains("DNS filtering ON"));
        assert!(
            combined.contains("nftables NOT applied"),
            "doctor must NOT claim nftables enforcement when `nft` is missing: {combined}"
        );
    }

    #[tokio::test]
    async fn doctor_surfaces_recent_audit_errors() {
        // Group + wiring present, but the audit list returns a failing row.
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([
                {
                    "command": "groups.create",
                    "result": "error",
                    "error_code": "invalid_input"
                }
            ])),
            Ok(json!([])),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(combined.contains("WARN"));
        assert!(combined.contains("audit-errors"));
        assert!(combined.contains("groups.create -> invalid_input"));
    }

    #[tokio::test]
    async fn doctor_dropped_messages_warn() {
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([{"id": "drop-1"}, {"id": "drop-2"}])),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(combined.contains("dropped-messages"));
        assert!(combined.contains("2 dropped"));
    }

    #[tokio::test]
    async fn doctor_json_mode_emits_structured_payload() {
        let t = SequencedTransport::new(vec![
            Ok(json!([{"id": "ag-1"}])),
            Ok(json!([{"id": "w-1"}])),
            Ok(json!([])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let out = run_cli(["cclaw", "--json", "doctor"], &t).await;
        let payload: serde_json::Value =
            serde_json::from_str(out.stdout.trim()).expect("doctor --json must be parseable JSON");
        assert_eq!(payload["status"], "ok");
        let checks = payload["checks"].as_array().unwrap();
        assert!(checks.iter().any(|c| c["name"] == "host-reachable"));
        assert!(checks.iter().any(|c| c["name"] == "agent-group"));
        // Every entry must have a level + detail; failing rows must have fix.
        for c in checks {
            assert!(c["level"].is_string());
            assert!(c["detail"].is_string());
            if c["level"] == "fail" {
                assert!(c["fix"].is_string());
            }
        }
    }

    #[tokio::test]
    async fn doctor_json_fail_status_when_any_check_fails() {
        let t = SequencedTransport::new(vec![Err(ClientError::Timeout)]);
        let out = run_cli(["cclaw", "--json", "doctor"], &t).await;
        // FAIL paths go to stderr per RunOutput::failure convention.
        let payload: serde_json::Value =
            serde_json::from_str(out.stderr.trim()).expect("--json fail must still emit JSON");
        assert_eq!(payload["status"], "fail");
    }

    // --- M21 O1: Wave-1 recovery-matrix doctor rows --------------------
    // Each new check gets a healthy AND a failing unit case (and the
    // failing case's `fix:` line names a real command / config key), plus
    // two full-flow tests proving the rows appear and the existing rows are
    // untouched.

    #[test]
    fn container_runtime_check_ok_and_fail() {
        let ok = runtime_check_from(&RuntimeProbe::Reachable("/var/run/docker.sock".into()));
        assert_eq!(ok.level, CheckLevel::Ok);
        assert_eq!(ok.name, "container-runtime");

        let bad = runtime_check_from(&RuntimeProbe::Unreachable(
            "/var/run/docker.sock".into(),
            "connection refused".into(),
        ));
        assert_eq!(bad.level, CheckLevel::Fail);
        let fix = bad.fix.expect("fail row must carry a fix");
        assert!(
            fix.contains("docker info"),
            "fix names a real command: {fix}"
        );
    }

    #[test]
    fn host_loops_check_ok_dead_and_degraded() {
        let healthy = json!({
            "degraded": false,
            "loops": [
                {"name": "sweep", "alive": true, "degraded": false},
                {"name": "delivery", "alive": true, "degraded": false},
            ],
        });
        let ok = host_loops_check(&healthy);
        assert_eq!(ok.level, CheckLevel::Ok);
        assert!(ok.detail.contains("2 background loop(s) alive"));

        let dead = json!({
            "degraded": false,
            "loops": [{"name": "sweep", "alive": false, "degraded": false}],
        });
        let f = host_loops_check(&dead);
        assert_eq!(f.level, CheckLevel::Fail);
        assert!(f.detail.contains("sweep (dead)"));
        let fix = f.fix.expect("fail row must carry a fix");
        assert!(
            fix.contains("copperclaw stop && copperclaw start"),
            "fix names a real command: {fix}"
        );

        // The supervisor-wide degraded flag alone is a FAIL even when every
        // listed loop reports alive.
        let degraded = json!({
            "degraded": true,
            "loops": [{"name": "sweep", "alive": true, "degraded": false}],
        });
        assert_eq!(host_loops_check(&degraded).level, CheckLevel::Fail);
    }

    #[test]
    fn stuck_sessions_check_fresh_and_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let ag = "11111111-1111-1111-1111-111111111111";
        let sess = "22222222-2222-2222-2222-222222222222";
        let sess_dir = root.join("sessions").join(ag).join(sess);
        std::fs::create_dir_all(&sess_dir).unwrap();
        std::fs::write(sess_dir.join(".heartbeat"), b"").unwrap();
        let running = vec![json!({"id": sess, "agent_group_id": ag})];

        // Just-written heartbeat, real `now` → fresh → OK.
        let ok = stuck_sessions_check(root, &running, std::time::SystemTime::now());
        assert_eq!(ok.level, CheckLevel::Ok, "detail: {}", ok.detail);

        // Advance `now` well past the staleness ceiling → the same heartbeat
        // reads as stale → FAIL with a real recovery command.
        let later = std::time::SystemTime::now()
            + std::time::Duration::from_millis(DOCTOR_HEARTBEAT_STALE_MS + 60_000);
        let f = stuck_sessions_check(root, &running, later);
        assert_eq!(f.level, CheckLevel::Fail);
        assert!(f.detail.contains(sess));
        let fix = f.fix.expect("fail row must carry a fix");
        assert!(
            fix.contains("cclaw sessions delete") || fix.contains("cclaw groups restart"),
            "fix names a real command: {fix}"
        );
    }

    #[test]
    fn stuck_sessions_missing_heartbeat_is_not_counted() {
        // A running session that has not yet written a heartbeat must not be
        // reported stuck (avoid crying wolf on cold starts).
        let tmp = tempfile::tempdir().unwrap();
        let running = vec![json!({
            "id": "22222222-2222-2222-2222-222222222222",
            "agent_group_id": "11111111-1111-1111-1111-111111111111",
        })];
        let now = std::time::SystemTime::now()
            + std::time::Duration::from_millis(DOCTOR_HEARTBEAT_STALE_MS + 60_000);
        assert_eq!(
            stuck_sessions_check(tmp.path(), &running, now).level,
            CheckLevel::Ok
        );
    }

    #[test]
    fn provider_chain_states_and_row() {
        let now = chrono::Utc::now();
        // No chain → NoChain; aggregated → WARN with the config pointer.
        let none = json!({"chain": null, "health": []});
        assert_eq!(group_chain_state(&none, now), ChainState::NoChain);
        let warn =
            provider_chain_check(&[("ag-1".into(), ChainState::NoChain)]).expect("row present");
        assert_eq!(warn.level, CheckLevel::Warn);
        let fix = warn.fix.expect("warn row carries a config pointer");
        assert!(
            fix.contains("cclaw groups provider set-chain"),
            "fix names a real command: {fix}"
        );

        // Configured chain, no failing health rows → Healthy → OK.
        let healthy = json!({
            "chain": [{"provider": "anthropic", "model": "m1"}],
            "health": [],
        });
        assert_eq!(group_chain_state(&healthy, now), ChainState::Healthy);
        assert_eq!(
            provider_chain_check(&[("ag-1".into(), ChainState::Healthy)])
                .unwrap()
                .level,
            CheckLevel::Ok
        );

        // Every entry degraded and still cooling → AllUnhealthy → FAIL.
        let future = (now + chrono::Duration::minutes(5)).to_rfc3339();
        let dead = json!({
            "chain": [{"provider": "anthropic", "model": "m1"}],
            "health": [{"provider": "anthropic", "model": "m1", "status": "down",
                        "cooldown_until": future}],
        });
        assert_eq!(group_chain_state(&dead, now), ChainState::AllUnhealthy);
        let f = provider_chain_check(&[("ag-1".into(), ChainState::AllUnhealthy)]).unwrap();
        assert_eq!(f.level, CheckLevel::Fail);
        let fix = f.fix.expect("fail row must carry a fix");
        assert!(
            fix.contains("cclaw groups provider status"),
            "fix names a real command: {fix}"
        );

        // A degraded entry whose cooldown has elapsed is re-probe-eligible →
        // treated healthy again.
        let past = (now - chrono::Duration::minutes(5)).to_rfc3339();
        let recovered = json!({
            "chain": [{"provider": "anthropic", "model": "m1"}],
            "health": [{"provider": "anthropic", "model": "m1", "status": "down",
                        "cooldown_until": past}],
        });
        assert_eq!(group_chain_state(&recovered, now), ChainState::Healthy);
    }

    #[test]
    fn provider_chain_check_empty_states_is_skipped() {
        assert!(provider_chain_check(&[]).is_none());
    }

    #[test]
    fn dead_letter_check_empty_and_backlog() {
        assert_eq!(
            dead_letter_check(&json!([])).level,
            CheckLevel::Ok,
            "empty backlog is OK"
        );

        let backlog = json!([
            {"id": "d1", "last_error": "no_adapter: cli"},
            {"id": "d2", "last_error": "timeout"},
        ]);
        let f = dead_letter_check(&backlog);
        assert_eq!(f.level, CheckLevel::Fail);
        assert!(f.detail.contains("2 undeliverable"));
        assert!(f.detail.contains("1 with no channel adapter"));
        let fix = f.fix.expect("fail row must carry a fix");
        assert!(
            fix.contains("cclaw dropped-messages replay"),
            "fix names a real command: {fix}"
        );
    }

    #[test]
    fn db_integrity_check_clean_quarantine_and_corrupt_central() {
        // Clean data root: no central DB, no sessions → OK.
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            db_integrity_check(tmp.path()).level,
            CheckLevel::Ok,
            "clean install is OK"
        );

        // A quarantine sidecar → FAIL surfacing the detail + a reset command.
        // The marker is a SIBLING of the session dir (host-only), named
        // `<session_uuid>.quarantined`, per O2's hardened on-disk contract.
        let sess = "33333333-3333-3333-3333-333333333333";
        let agent_dir = tmp.path().join("sessions").join("ag");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join(format!("{sess}.quarantined")),
            r#"{"reason":"quick_check","db":"outbound.db","detail":"database disk image is malformed","detected_at":"2026-07-17T00:00:00Z","future_key":"ignored"}"#,
        )
        .unwrap();
        let f = db_integrity_check(tmp.path());
        assert_eq!(f.level, CheckLevel::Fail);
        assert!(f.detail.contains(sess));
        assert!(f.detail.contains("database disk image is malformed"));
        let fix = f.fix.expect("fail row must carry a fix");
        assert!(
            fix.contains("cclaw sessions delete"),
            "fix names a real command: {fix}"
        );

        // A corrupt central DB takes precedence and names the restore command.
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp2.path().join("copperclaw.db"),
            b"not a sqlite database at all",
        )
        .unwrap();
        let cf = db_integrity_check(tmp2.path());
        assert_eq!(cf.level, CheckLevel::Fail);
        assert!(cf.detail.contains("central DB"));
        let fix = cf.fix.expect("fail row must carry a fix");
        assert!(
            fix.contains("cclaw db restore"),
            "fix names a real command: {fix}"
        );
    }

    /// Full-flow: with a live supervisor, running sessions, provider chains,
    /// a dead-letter backlog and a quarantined session all present, every new
    /// FAIL/WARN row appears with its `fix:` line — and the existing rows are
    /// still emitted byte-for-byte (host-reachable / agent-group / etc.).
    #[tokio::test]
    async fn doctor_full_flow_surfaces_all_new_failing_rows() {
        set_runtime_override(RuntimeProbe::Unreachable(
            "/var/run/docker.sock".into(),
            "connection refused".into(),
        ));
        let tmp = tempfile::tempdir().unwrap();
        // A quarantined session for the db-integrity row: sibling marker
        // (`<session_uuid>.quarantined`) in the agent-group dir.
        let agent_dir = tmp.path().join("sessions").join("ag");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("sess-q.quarantined"),
            r#"{"reason":"quick_check"}"#,
        )
        .unwrap();
        set_data_root_override(Some(tmp.path().to_path_buf()));

        let future = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        let t = MapTransport::new(vec![
            ("groups.list", Ok(json!([{"id": "ag-1"}]))),
            ("wirings.list", Ok(json!([{"id": "w-1"}]))),
            ("sessions.list", Ok(json!([]))), // active list (row 4) + running list both map here
            ("audit.list", Ok(json!([]))),
            ("dropped-messages.list", Ok(json!([]))),
            (
                "host.status",
                Ok(json!({"degraded": false,
                          "loops": [{"name": "sweep", "alive": false, "degraded": false}]})),
            ),
            (
                "groups.provider.status",
                Ok(json!({"chain": [{"provider": "anthropic", "model": "m1"}],
                          "health": [{"provider": "anthropic", "model": "m1",
                                      "status": "down", "cooldown_until": future}]})),
            ),
            (
                "dropped-messages.outbound-list",
                Ok(json!([{"id": "d1", "last_error": "no_adapter: cli"}])),
            ),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        // Existing rows still present.
        assert!(combined.contains("host-reachable"));
        assert!(combined.contains("agent-group"));
        // New rows, all failing here.
        assert!(combined.contains("container-runtime"));
        assert!(combined.contains("host-loops"));
        assert!(combined.contains("provider-chain"));
        assert!(combined.contains("dead-letter"));
        assert!(combined.contains("db-integrity"));
        assert!(combined.contains("FAIL"));
        assert!(combined.contains("docker info"));
        assert!(combined.contains("cclaw dropped-messages replay"));
        // FAIL sends the whole report to stderr.
        assert!(out.stderr.contains("FAIL"));

        // Reset thread-locals so sibling tests on a reused thread see defaults.
        set_runtime_override(RuntimeProbe::Reachable("<test>".into()));
        set_data_root_override(None);
    }

    /// Full-flow healthy path: reachable runtime, live loops, a healthy
    /// provider chain, no backlog, no quarantine → every new row is OK/skip
    /// and the report exits clean.
    #[tokio::test]
    async fn doctor_full_flow_all_new_rows_healthy() {
        set_runtime_override(RuntimeProbe::Reachable("/var/run/docker.sock".into()));
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();
        set_data_root_override(Some(tmp.path().to_path_buf()));

        let t = MapTransport::new(vec![
            ("groups.list", Ok(json!([{"id": "ag-1"}]))),
            ("wirings.list", Ok(json!([{"id": "w-1"}]))),
            ("sessions.list", Ok(json!([]))),
            ("audit.list", Ok(json!([]))),
            ("dropped-messages.list", Ok(json!([]))),
            (
                "host.status",
                Ok(json!({"degraded": false,
                          "loops": [{"name": "sweep", "alive": true, "degraded": false}]})),
            ),
            (
                "groups.provider.status",
                Ok(json!({"chain": [{"provider": "anthropic", "model": "m1"}], "health": []})),
            ),
            ("dropped-messages.outbound-list", Ok(json!([]))),
        ]);
        let out = run_cli(["cclaw", "doctor"], &t).await;
        assert!(
            out.stderr.is_empty(),
            "healthy doctor must not FAIL: {:?}",
            out.stderr
        );
        assert!(out.stdout.contains("container-runtime"));
        assert!(out.stdout.contains("host-loops"));
        assert!(out.stdout.contains("provider-chain"));
        assert!(out.stdout.contains("dead-letter"));
        assert!(out.stdout.contains("db-integrity"));
        assert!(!out.stdout.contains("FAIL"));

        set_data_root_override(None);
    }

    // --- chat rendering ----------------------------------------------------

    fn plain_state() -> ChatRenderState {
        ChatRenderState::default()
    }

    fn render_plain(frame: &CliFrame, state: &mut ChatRenderState) -> RenderedChatLine {
        render_chat_frame(frame, Palette::plain(), state)
    }

    #[test]
    fn chat_render_legacy_line_passes_through_verbatim() {
        let mut state = plain_state();
        let out = render_chat_line("agent> hello there\n", Palette::plain(), &mut state);
        assert_eq!(out.lines, vec!["agent> hello there".to_string()]);
        assert!(!out.status_changed);
    }

    #[test]
    fn chat_render_malformed_json_line_passes_through_verbatim() {
        let mut state = plain_state();
        let out = render_chat_line("{not json}\n", Palette::plain(), &mut state);
        assert_eq!(out.lines, vec!["{not json}".to_string()]);
    }

    #[test]
    fn chat_render_empty_line_renders_nothing() {
        let mut state = plain_state();
        let out = render_chat_line("\n", Palette::plain(), &mut state);
        assert_eq!(out, RenderedChatLine::default());
    }

    #[test]
    fn chat_render_chat_frame_text_and_files() {
        let mut state = plain_state();
        let frame = CliFrame::Chat {
            text: "hello\nworld".into(),
            files: vec!["a.txt".into(), "b.png".into()],
        };
        let out = render_plain(&frame, &mut state);
        assert_eq!(
            out.lines,
            vec![
                "hello".to_string(),
                "world".to_string(),
                "[files: a.txt, b.png]".to_string(),
            ]
        );
    }

    #[test]
    fn chat_render_card_frame_title_body_fields() {
        let mut state = plain_state();
        let frame = CliFrame::Card {
            card: copperclaw_channels_core::Card {
                title: Some("Deploy report".into()),
                body: Some("all green".into()),
                fields: vec![copperclaw_channels_core::CardField {
                    label: "env".into(),
                    value: "prod".into(),
                    inline: false,
                }],
                buttons: vec![],
                image_url: None,
            },
        };
        let out = render_plain(&frame, &mut state);
        assert_eq!(
            out.lines,
            vec![
                "Deploy report".to_string(),
                "all green".to_string(),
                "env: prod".to_string(),
            ]
        );
    }

    #[test]
    fn chat_render_collapsible_frame_summary_then_body() {
        let mut state = plain_state();
        let frame = CliFrame::Collapsible {
            text: "line one\nline two".into(),
            summary: "2 lines of output".into(),
            preview_lines: vec!["line one".into()],
        };
        let out = render_plain(&frame, &mut state);
        assert_eq!(
            out.lines,
            vec![
                "2 lines of output".to_string(),
                "line one".to_string(),
                "line two".to_string(),
            ]
        );
    }

    #[test]
    fn chat_render_breadcrumb_single_chip_running_then_done() {
        let mut state = plain_state();
        let running = CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("shell").with_detail("cargo check"),
        };
        let out = render_plain(&running, &mut state);
        // RAIL binding: running marker is U+25CB WHITE CIRCLE.
        assert_eq!(out.lines, vec!["\u{25CB} shell(cargo check)".to_string()]);

        // The completion edit frame prints ONLY the result line — the
        // step line is already in scrollback.
        let done = CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("shell")
                .with_detail("cargo check")
                .finished(true, Some("passed in 3.1s".into())),
        };
        let out = render_plain(&done, &mut state);
        assert_eq!(
            out.lines,
            vec!["  \u{23BF} passed in 3.1s".to_string()],
            "completion edit must print only the result line"
        );

        // An exact duplicate edit frame renders nothing.
        let out = render_plain(&done, &mut state);
        assert!(out.lines.is_empty());
    }

    #[test]
    fn chat_render_breadcrumb_fresh_done_chip_prints_step_and_result() {
        let mut state = plain_state();
        let frame = CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("edit_file")
                .with_detail("src/lib.rs")
                .finished(false, Some("permission denied".into())),
        };
        let out = render_plain(&frame, &mut state);
        // Failed marker is U+00D7 MULTIPLICATION SIGN on the RAIL binding.
        assert_eq!(
            out.lines,
            vec![
                "\u{D7} edit_file(src/lib.rs)".to_string(),
                "  \u{23BF} permission denied".to_string(),
            ]
        );
    }

    #[test]
    fn chat_render_hud_frames_dedupe_steps_and_track_status_line() {
        let mut state = plain_state();
        let step1 = Breadcrumb::running("shell")
            .with_detail("cargo check")
            .finished(true, Some("ok".into()));
        let step2 = Breadcrumb::running("edit_file").with_detail("src/lib.rs");
        let hud1 = CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("task")
                .with_steps(vec![step1.clone(), step2.clone()])
                .with_detail("building"),
        };
        let out = render_plain(&hud1, &mut state);
        assert_eq!(
            out.lines,
            vec![
                "\u{23FA} shell(cargo check)".to_string(),
                "  \u{23BF} ok".to_string(),
                "\u{25CB} edit_file(src/lib.rs)".to_string(),
            ]
        );
        assert!(out.status_changed);
        assert_eq!(state.status_line.as_deref(), Some("task(building)"));

        // Edit frame: step2 completed, one NEW step appended, summary set.
        let step2_done = step2.finished(true, Some("wrote 12 lines".into()));
        let step3 = Breadcrumb::running("web_search").with_detail("rust select");
        let hud2 = CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb {
                summary: Some("Exploring (0:12)".into()),
                ..Breadcrumb::running("task").with_steps(vec![step1, step2_done, step3])
            },
        };
        let out = render_plain(&hud2, &mut state);
        assert_eq!(
            out.lines,
            vec![
                "  \u{23BF} wrote 12 lines".to_string(),
                "\u{25CB} web_search(rust select)".to_string(),
            ],
            "repeat frames must print only late results plus NEW steps"
        );
        assert!(out.status_changed);
        assert_eq!(state.status_line.as_deref(), Some("Exploring (0:12)"));

        // Same frame again: nothing new, status unchanged.
        let out = render_plain(&hud2, &mut state);
        assert!(out.lines.is_empty());
        assert!(!out.status_changed);
    }

    #[test]
    fn chat_render_hud_new_message_resets_step_dedupe() {
        let mut state = plain_state();
        let hud1 = CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("task")
                .with_steps(vec![Breadcrumb::running("shell").with_detail("a")]),
        };
        assert_eq!(render_plain(&hud1, &mut state).lines.len(), 1);
        // A frame whose steps do NOT extend the rendered prefix is a new
        // logical HUD message: render from scratch.
        let hud2 = CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("task")
                .with_steps(vec![Breadcrumb::running("shell").with_detail("b")]),
        };
        let out = render_plain(&hud2, &mut state);
        assert_eq!(out.lines, vec!["\u{25CB} shell(b)".to_string()]);
    }

    #[test]
    fn chat_render_todo_list_checkboxes() {
        let mut state = plain_state();
        let frame = CliFrame::TodoList {
            todo_list: copperclaw_channels_core::TodoList {
                title: Some("Plan".into()),
                items: vec![
                    copperclaw_channels_core::TodoListItem {
                        id: 1,
                        text: "write tests".into(),
                        status: TodoItemStatus::Completed,
                        blocked_reason: None,
                    },
                    copperclaw_channels_core::TodoListItem {
                        id: 2,
                        text: "run gate".into(),
                        status: TodoItemStatus::InProgress,
                        blocked_reason: None,
                    },
                    copperclaw_channels_core::TodoListItem {
                        id: 3,
                        text: "deploy".into(),
                        status: TodoItemStatus::Blocked,
                        blocked_reason: Some("no creds".into()),
                    },
                    copperclaw_channels_core::TodoListItem {
                        id: 4,
                        text: "announce".into(),
                        status: TodoItemStatus::Pending,
                        blocked_reason: None,
                    },
                ],
            },
        };
        let out = render_plain(&frame, &mut state);
        assert_eq!(
            out.lines,
            vec![
                "Plan".to_string(),
                "[x] write tests".to_string(),
                "[~] run gate".to_string(),
                "[!] deploy — no creds".to_string(),
                "[ ] announce".to_string(),
            ]
        );
    }

    #[test]
    fn chat_render_thinking_dimmed_and_redacted_placeholder() {
        let mut state = plain_state();
        let visible = CliFrame::Thinking {
            thinking: copperclaw_channels_core::ThinkingBlock::visible("hmm\nokay"),
        };
        let out = render_plain(&visible, &mut state);
        assert_eq!(out.lines, vec!["hmm".to_string(), "okay".to_string()]);

        let redacted = CliFrame::Thinking {
            thinking: copperclaw_channels_core::ThinkingBlock::redacted("opaque-blob"),
        };
        let out = render_plain(&redacted, &mut state);
        assert_eq!(out.lines, vec!["[thinking redacted]".to_string()]);
        assert!(
            !out.lines[0].contains("opaque-blob"),
            "redacted blob must never render"
        );
    }

    #[test]
    fn chat_render_diff_frame_header_hunks_and_truncation() {
        let mut state = plain_state();
        let frame = CliFrame::Diff {
            diff: copperclaw_channels_core::DiffCard {
                path: "src/main.rs".into(),
                language: Some("rust".into()),
                hunks: vec![copperclaw_channels_core::DiffHunk {
                    old_start: 1,
                    old_lines: 2,
                    new_start: 1,
                    new_lines: 2,
                    lines: vec![
                        copperclaw_channels_core::DiffLine {
                            kind: DiffLineKind::Context,
                            text: "fn main() {".into(),
                        },
                        copperclaw_channels_core::DiffLine {
                            kind: DiffLineKind::Remove,
                            text: "    old();".into(),
                        },
                        copperclaw_channels_core::DiffLine {
                            kind: DiffLineKind::Add,
                            text: "    new();".into(),
                        },
                    ],
                }],
                added: 1,
                removed: 1,
                truncated: true,
            },
        };
        let out = render_plain(&frame, &mut state);
        assert_eq!(
            out.lines,
            vec![
                "src/main.rs (+1 -1)".to_string(),
                "@@ -1,2 +1,2 @@".to_string(),
                " fn main() {".to_string(),
                "-    old();".to_string(),
                "+    new();".to_string(),
                "[diff truncated]".to_string(),
            ]
        );
    }

    #[test]
    fn chat_render_error_frame_headline_details_retry() {
        let mut state = plain_state();
        let frame = CliFrame::Error {
            error: copperclaw_channels_core::ErrorCard {
                title: "Something went wrong".into(),
                summary: "provider rate limited".into(),
                kind: copperclaw_channels_core::ErrorCardKind::Provider,
                details: Some("HTTP 429".into()),
                retryable: true,
            },
        };
        let out = render_plain(&frame, &mut state);
        assert_eq!(
            out.lines,
            vec![
                "[provider error] Something went wrong: provider rate limited".to_string(),
                "HTTP 429".to_string(),
                "(will retry automatically)".to_string(),
            ]
        );
    }

    #[test]
    fn chat_render_rail_lines_styled_when_colored() {
        // Palette construction is pure — cfg(test) only pins the
        // is_terminal resolver, not palette rendering — so the "on" side
        // of the rail styling is testable in-process.
        let p = Palette::new(true);
        let mut state = plain_state();
        let frame = CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("shell").with_detail("cargo check"),
        };
        let out = render_chat_frame(&frame, p, &mut state);
        let line = &out.lines[0];
        // Marker colored yellow (running), tool name bold, detail dim.
        assert!(line.contains(&p.warn("\u{25CB}")));
        assert!(line.contains(&p.header("shell")));
        assert!(line.contains(&p.dim("cargo check")));

        // Failed completion: red marker on a fresh chip, red summary.
        let mut state = plain_state();
        let failed = CliFrame::Breadcrumb {
            breadcrumb: Breadcrumb::running("shell")
                .with_detail("cargo test")
                .finished(false, Some("2 failed".into())),
        };
        let out = render_chat_frame(&failed, p, &mut state);
        assert!(out.lines[0].contains(&p.fail("\u{D7}")));
        assert!(out.lines[1].contains(&p.fail("2 failed")));
    }

    #[test]
    fn chat_render_diff_and_todo_styled_when_colored() {
        let p = Palette::new(true);
        let mut state = plain_state();
        let diff = CliFrame::Diff {
            diff: copperclaw_channels_core::DiffCard {
                path: "a.rs".into(),
                language: None,
                hunks: vec![copperclaw_channels_core::DiffHunk {
                    old_start: 1,
                    old_lines: 1,
                    new_start: 1,
                    new_lines: 1,
                    lines: vec![
                        copperclaw_channels_core::DiffLine {
                            kind: DiffLineKind::Add,
                            text: "x".into(),
                        },
                        copperclaw_channels_core::DiffLine {
                            kind: DiffLineKind::Remove,
                            text: "y".into(),
                        },
                    ],
                }],
                added: 1,
                removed: 1,
                truncated: false,
            },
        };
        let out = render_chat_frame(&diff, p, &mut state);
        assert!(out.lines.contains(&p.diff_add("+x")));
        assert!(out.lines.contains(&p.diff_remove("-y")));

        let todo = CliFrame::TodoList {
            todo_list: copperclaw_channels_core::TodoList {
                title: None,
                items: vec![copperclaw_channels_core::TodoListItem {
                    id: 1,
                    text: "done thing".into(),
                    status: TodoItemStatus::Completed,
                    blocked_reason: None,
                }],
            },
        };
        let out = render_chat_frame(&todo, p, &mut state);
        assert_eq!(out.lines, vec![format!("[x] {}", p.strike("done thing"))]);
    }

    // --- chat --------------------------------------------------------------

    #[tokio::test]
    async fn chat_no_autostart_with_missing_fifo_errors_without_spawning() {
        // Point at a definitely-missing fifo + log so `run_chat` aborts
        // before any blocking I/O. `--no-autostart` should suppress the
        // auto-launch path and surface a clear "host not running" hint.
        let t = SequencedTransport::new(vec![]);
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("copperclaw-test.fifo");
        let log = tmp.path().join("copperclaw-test.log");
        let out = run_cli(
            [
                "cclaw",
                "chat",
                "--fifo",
                fifo.to_str().unwrap(),
                "--log",
                log.to_str().unwrap(),
                "--no-autostart",
            ],
            &t,
        )
        .await;
        assert!(out.stdout.is_empty());
        assert!(
            out.stderr.contains("no FIFO"),
            "stderr was: {:?}",
            out.stderr,
        );
        assert!(
            out.stderr.contains("copperclaw start") || out.stderr.contains("copperclaw run"),
            "stderr should hint at how to start the host: {:?}",
            out.stderr,
        );
        // Crucially we never reached the transport — chat is pure file I/O.
        assert!(t.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn chat_autostart_not_feasible_when_path_lacks_copperclaw() {
        // Strip PATH so `which_copperclaw` returns None. The autostart
        // path should fall through to a clear "could not auto-start"
        // error rather than the "no FIFO" hint.
        //
        // NOTE: this test relies on the process env's PATH not
        // containing an `copperclaw` binary that we accidentally launch.
        // We narrow the blast radius by passing an empty PATH via the
        // child env? No — `try_autostart_host` reads the host process
        // env directly. So instead we just check the *shape* of the
        // error: either NotFeasible (no binary) or FailedToBoot (it
        // tried but the binary isn't this host's binary). Either way
        // the message names the FIFO path we asked for.
        let t = SequencedTransport::new(vec![]);
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("copperclaw-test.fifo");
        let log = tmp.path().join("copperclaw-test.log");
        let out = run_cli(
            [
                "cclaw",
                "chat",
                "--fifo",
                fifo.to_str().unwrap(),
                "--log",
                log.to_str().unwrap(),
                "--no-autostart",
            ],
            &t,
        )
        .await;
        assert!(out.stderr.contains(fifo.to_str().unwrap()));
    }

    // ---- dashboard -----------------------------------------------------

    /// Transport stub that dispatches by command name. Required because
    /// the dashboard fans calls out via `tokio::join!`, which polls in
    /// declaration order but recording them by name is robust either way.
    struct MapTransport {
        responses: std::collections::HashMap<String, Result<serde_json::Value, ClientError>>,
        calls: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl MapTransport {
        fn new(pairs: Vec<(&str, Result<serde_json::Value, ClientError>)>) -> Self {
            Self {
                responses: pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl CallTransport for MapTransport {
        async fn call(
            &self,
            command: &str,
            args: serde_json::Value,
            _caller: Caller,
        ) -> Result<serde_json::Value, ClientError> {
            self.calls.lock().unwrap().push((command.to_string(), args));
            match self.responses.get(command) {
                Some(Ok(v)) => Ok(v.clone()),
                Some(Err(_)) => {
                    // ClientError isn't Clone; rebuild a representative one
                    // for each repeated lookup.
                    Err(ClientError::Remote(ErrorPayload::new(
                        "stub-err",
                        "stubbed error",
                    )))
                }
                None => Err(ClientError::Remote(ErrorPayload::new(
                    "not-stubbed",
                    format!("MapTransport has no stub for {command}"),
                ))),
            }
        }
    }

    #[tokio::test]
    async fn no_args_runs_dashboard_with_expected_sections() {
        let t = MapTransport::new(vec![
            (
                "groups.list",
                Ok(json!([{"id": "ag-1", "name": "first", "agent_provider": "anthropic"}])),
            ),
            (
                "wirings.list",
                Ok(
                    json!([{"id": "w-1", "messaging_group_id": "mg-1", "agent_group_id": "ag-1", "engage": "pattern"}]),
                ),
            ),
            ("sessions.list", Ok(json!([]))),
            ("audit.list", Ok(json!([{"id":"a-1"}, {"id":"a-2"}]))),
            ("dropped-messages.list", Ok(json!([]))),
            (
                "usage.rollup",
                Ok(json!([{"agent_group_id": "ag-1", "total_tokens": 1234}])),
            ),
        ]);
        let out = run_cli(["cclaw"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(out.stdout.contains("copperclaw at "));
        assert!(out.stdout.contains("agent groups (1)"));
        assert!(out.stdout.contains("ag-1"));
        assert!(out.stdout.contains("wirings (1)"));
        assert!(out.stdout.contains("active sessions (0)"));
        assert!(out.stdout.contains("recent activity (last 1h)"));
        assert!(out.stdout.contains("2 mutations"));
        assert!(out.stdout.contains("1234 tokens"));
        assert!(out.stdout.contains("suggested next:"));
        // Confirm all six read endpoints were hit.
        let calls: Vec<String> = t
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(c, _)| c.clone())
            .collect();
        for expected in [
            "groups.list",
            "wirings.list",
            "sessions.list",
            "audit.list",
            "dropped-messages.list",
            "usage.rollup",
        ] {
            assert!(calls.iter().any(|c| c == expected), "missing {expected}");
        }
    }

    #[tokio::test]
    async fn dashboard_json_emits_single_object() {
        let t = MapTransport::new(vec![
            ("groups.list", Ok(json!([]))),
            ("wirings.list", Ok(json!([]))),
            ("sessions.list", Ok(json!([]))),
            ("audit.list", Ok(json!([]))),
            ("dropped-messages.list", Ok(json!([]))),
            ("usage.rollup", Ok(json!([]))),
        ]);
        let out = run_cli(["cclaw", "--json"], &t).await;
        assert!(out.stderr.is_empty());
        let parsed: serde_json::Value =
            serde_json::from_str(out.stdout.trim()).expect("valid json");
        // Required top-level keys.
        for key in [
            "install_root",
            "agent_groups",
            "wirings",
            "active_sessions",
            "recent_activity",
            "suggestions",
        ] {
            assert!(parsed.get(key).is_some(), "missing key {key}");
        }
    }

    #[test]
    fn format_cost_dollars_rounds_half_up_to_cents() {
        // A genuine priced zero (e.g. ollama's $0/MTok) renders "$0.00".
        assert_eq!(format_cost_dollars(0), "$0.00");
        // Priced sub-cent value also collapses to "$0.00" — acceptable
        // because it *was* priced; unknown is the em dash, not zero.
        assert_eq!(format_cost_dollars(3_300), "$0.00");
        assert_eq!(format_cost_dollars(420_000), "$0.42");
        // Half rounds up: 42.5 cents -> 43.
        assert_eq!(format_cost_dollars(425_000), "$0.43");
        assert_eq!(format_cost_dollars(424_999), "$0.42");
        // Dollars are not zero-padded; cents always two digits.
        assert_eq!(format_cost_dollars(1_234_567_890), "$1234.57");
    }

    #[tokio::test]
    async fn usage_table_has_cost_column_and_pricing_footer() {
        let t = StubTransport::ok(json!([
            {
                "agent_group_id": "ag-priced",
                "turns": 3,
                "input_tokens": 100,
                "output_tokens": 200,
                "total_tokens": 300,
                "first_at": "2026-07-20T00:00:00+00:00",
                "last_at": "2026-07-20T01:00:00+00:00",
                "models": [{
                    "model": "claude-sonnet-4-6",
                    "provider": "anthropic",
                    "turns": 3,
                    "input_tokens": 100,
                    "output_tokens": 200,
                    "cost_micros": 420_000,
                }],
                "cost_micros": 420_000,
                "pricing_as_of": "2026-07-20",
            },
            {
                "agent_group_id": "ag-unknown",
                "turns": 1,
                "input_tokens": 10,
                "output_tokens": 20,
                "total_tokens": 30,
                "first_at": "2026-07-20T00:00:00+00:00",
                "last_at": "2026-07-20T01:00:00+00:00",
                "models": [{
                    "model": "mystery-model",
                    "provider": "ollama-cloud",
                    "turns": 1,
                    "input_tokens": 10,
                    "output_tokens": 20,
                    "cost_micros": null,
                }],
                "cost_micros": null,
                "pricing_as_of": "2026-07-20",
            },
        ]));
        let out = run_cli(["cclaw", "usage"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        // (a) priced group renders dollars, (b) unpriced renders an em
        // dash — never a confidently wrong "$0.00".
        assert!(out.stdout.contains("COST"), "stdout={:?}", out.stdout);
        assert!(out.stdout.contains("$0.42"), "stdout={:?}", out.stdout);
        assert!(out.stdout.contains(COST_UNKNOWN), "stdout={:?}", out.stdout);
        assert!(!out.stdout.contains("$0.00"), "stdout={:?}", out.stdout);
        // (c) pricing vintage is a single footer line, not a column.
        assert!(out.stdout.contains("pricing as of 2026-07-20"));
        assert!(!out.stdout.contains("PRICING_AS_OF"));
        // The nested per-model slices and raw micros stay off the table.
        assert!(!out.stdout.contains("cost_micros"));
        assert!(!out.stdout.contains("claude-sonnet-4-6"));
        assert!(!out.stdout.contains("420000"));
    }

    #[tokio::test]
    async fn usage_json_passes_handler_payload_through() {
        let payload = json!([{
            "agent_group_id": "ag-1",
            "turns": 1,
            "input_tokens": 100,
            "output_tokens": 200,
            "total_tokens": 300,
            "first_at": "2026-07-20T00:00:00+00:00",
            "last_at": "2026-07-20T01:00:00+00:00",
            "models": [{
                "model": "claude-sonnet-4-6",
                "provider": "anthropic",
                "turns": 1,
                "input_tokens": 100,
                "output_tokens": 200,
                "cost_micros": 3_300,
            }],
            "cost_micros": 3_300,
            "pricing_as_of": "2026-07-20",
        }]);
        let t = StubTransport::ok(payload.clone());
        let out = run_cli(["cclaw", "--json", "usage"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        let parsed: serde_json::Value =
            serde_json::from_str(out.stdout.trim()).expect("valid json");
        // Byte-for-byte handler payload: models, cost_micros, and
        // pricing_as_of all survive untouched.
        assert_eq!(parsed, payload);
    }

    #[tokio::test]
    async fn usage_table_without_pricing_keys_omits_footer() {
        // An older host that predates C3 sends rows without models /
        // cost_micros / pricing_as_of; the cost column degrades to em
        // dashes and the footer disappears rather than lying.
        let t = StubTransport::ok(json!([{
            "agent_group_id": "ag-old",
            "turns": 1,
            "input_tokens": 10,
            "output_tokens": 20,
            "total_tokens": 30,
            "first_at": "2026-07-20T00:00:00+00:00",
            "last_at": "2026-07-20T01:00:00+00:00",
        }]));
        let out = run_cli(["cclaw", "usage"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(out.stdout.contains(COST_UNKNOWN));
        assert!(!out.stdout.contains("pricing as of"));
    }

    #[tokio::test]
    async fn budgets_list_table_formats_caps_spend_and_state() {
        let t = StubTransport::ok(json!([
            {
                "agent_group_id": "ag-over",
                "daily_token_cap": 1_000_000,
                "daily_cost_cap": 0.01,
                "agent_turns_per_minute_cap": null,
                "agent_turns_per_hour_cap": null,
                "updated_at": "2026-07-20T00:00:00+00:00",
                "tokens_today": 1_500,
                "cost_today_micros": 10_500,
                "cost_today_unpriced_turns": 0,
                "over_daily_token_cap": false,
                "over_daily_cost_cap": true,
            },
            {
                "agent_group_id": "ag-partial",
                "daily_token_cap": null,
                "daily_cost_cap": 5.0,
                "agent_turns_per_minute_cap": null,
                "agent_turns_per_hour_cap": null,
                "updated_at": "2026-07-20T00:00:00+00:00",
                "tokens_today": 300,
                "cost_today_micros": 420_000,
                "cost_today_unpriced_turns": 2,
                "over_daily_token_cap": false,
                "over_daily_cost_cap": false,
            },
        ]));
        let out = run_cli(["cclaw", "budgets", "list"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        // Dollar-formatted cap and spend columns.
        assert!(out.stdout.contains("COST_TODAY"), "stdout={:?}", out.stdout);
        assert!(out.stdout.contains("$0.01"), "stdout={:?}", out.stdout);
        assert!(out.stdout.contains("$5.00"), "stdout={:?}", out.stdout);
        // Priced spend renders as dollars; a partial (unpriced turns
        // present) spend carries the trailing "+" floor marker.
        assert!(out.stdout.contains("$0.42+"), "stdout={:?}", out.stdout);
        // Breach state is visible at a glance.
        assert!(out.stdout.contains("over-cost"), "stdout={:?}", out.stdout);
        assert!(out.stdout.contains("ok"), "stdout={:?}", out.stdout);
        // Raw micros stay off the human table.
        assert!(!out.stdout.contains("10500"), "stdout={:?}", out.stdout);
    }

    #[tokio::test]
    async fn budgets_list_table_dashes_unset_cost_cap() {
        // Rows from a host without a cost cap (or an older host without
        // the spend columns) degrade to em dashes, never "$0.00".
        let t = StubTransport::ok(json!([{
            "agent_group_id": "ag-plain",
            "daily_token_cap": 42,
            "daily_cost_cap": null,
            "agent_turns_per_minute_cap": null,
            "agent_turns_per_hour_cap": null,
            "updated_at": "2026-07-20T00:00:00+00:00",
        }]));
        let out = run_cli(["cclaw", "budgets", "list"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(out.stdout.contains(COST_UNKNOWN), "stdout={:?}", out.stdout);
        assert!(!out.stdout.contains('$'), "stdout={:?}", out.stdout);
        // No breach flags -> state renders as ok.
        assert!(out.stdout.contains("ok"), "stdout={:?}", out.stdout);
    }

    #[tokio::test]
    async fn budgets_list_json_passes_handler_payload_through() {
        let payload = json!([{
            "agent_group_id": "ag-1",
            "daily_token_cap": 1000,
            "daily_cost_cap": 2.5,
            "agent_turns_per_minute_cap": null,
            "agent_turns_per_hour_cap": null,
            "updated_at": "2026-07-20T00:00:00+00:00",
            "tokens_today": 10,
            "cost_today_micros": 55,
            "cost_today_unpriced_turns": 0,
            "over_daily_token_cap": false,
            "over_daily_cost_cap": false,
        }]);
        let t = StubTransport::ok(payload.clone());
        let out = run_cli(["cclaw", "--json", "budgets", "list"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        let parsed: serde_json::Value =
            serde_json::from_str(out.stdout.trim()).expect("valid json");
        assert_eq!(parsed, payload);
    }

    #[tokio::test]
    async fn sessions_delete_round_trips_through_transport() {
        // Integration coverage for `cclaw sessions delete <id>`:
        // drive the CLI front-end against an in-process stub transport
        // and verify the parsed command/args reach the wire layer
        // unchanged.
        let t = StubTransport::ok(json!({
            "deleted": "00000000-0000-0000-0000-000000000001",
            "agent_group_id": "00000000-0000-0000-0000-0000000000aa",
            "directory_removed": true,
        }));
        let out = run_cli(
            [
                "cclaw",
                "sessions",
                "delete",
                "00000000-0000-0000-0000-000000000001",
            ],
            &t,
        )
        .await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        let captured = t.last_call.lock().unwrap();
        let (cmd, args, _caller) = captured.as_ref().unwrap();
        assert_eq!(cmd, "sessions.delete");
        assert_eq!(
            args,
            &json!({"id": "00000000-0000-0000-0000-000000000001", "force": false}),
        );
    }

    #[tokio::test]
    async fn sessions_delete_force_flag_carries_through() {
        let t = StubTransport::ok(json!({"deleted": "x"}));
        let out = run_cli(["cclaw", "sessions", "delete", "sess-1", "--force"], &t).await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        let captured = t.last_call.lock().unwrap();
        let (cmd, args, _caller) = captured.as_ref().unwrap();
        assert_eq!(cmd, "sessions.delete");
        assert_eq!(args["force"], json!(true));
    }

    #[tokio::test]
    async fn dashboard_unreachable_host_returns_friendly_error() {
        // Use a transport that returns an IO NotFound for every call.
        struct DeadTransport;
        #[async_trait::async_trait]
        impl CallTransport for DeadTransport {
            async fn call(
                &self,
                _command: &str,
                _args: serde_json::Value,
                _caller: Caller,
            ) -> Result<serde_json::Value, ClientError> {
                Err(ClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "no such socket",
                )))
            }
        }
        let out = run_cli(["cclaw"], &DeadTransport).await;
        assert!(out.stdout.is_empty());
        assert!(out.stderr.contains("host not running"));
    }

    #[tokio::test]
    async fn dashboard_zero_groups_suggests_quickstart() {
        let t = MapTransport::new(vec![
            ("groups.list", Ok(json!([]))),
            ("wirings.list", Ok(json!([]))),
            ("sessions.list", Ok(json!([]))),
            ("audit.list", Ok(json!([]))),
            ("dropped-messages.list", Ok(json!([]))),
            ("usage.rollup", Ok(json!([]))),
        ]);
        let out = run_cli(["cclaw"], &t).await;
        assert!(out.stdout.contains("cclaw quickstart cli"));
    }

    #[tokio::test]
    async fn dashboard_drops_suggests_dropped_messages() {
        let t = MapTransport::new(vec![
            ("groups.list", Ok(json!([{"id": "ag-1", "name": "x"}]))),
            ("wirings.list", Ok(json!([]))),
            ("sessions.list", Ok(json!([]))),
            ("audit.list", Ok(json!([]))),
            (
                "dropped-messages.list",
                Ok(json!([{"id":"d-1"},{"id":"d-2"}])),
            ),
            ("usage.rollup", Ok(json!([]))),
        ]);
        let out = run_cli(["cclaw"], &t).await;
        assert!(out.stdout.contains("cclaw dropped-messages list"));
    }

    // ---- groups config edit -------------------------------------------

    fn write_editor_script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let p = dir.join("editor.sh");
        std::fs::write(&p, body).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    #[tokio::test]
    async fn groups_config_edit_dry_run_no_changes_when_unedited() {
        // EDITOR `true` is a no-op exit-0 binary, so the temp file is
        // untouched. The workflow should print "no changes" and never
        // call groups.config.update.
        let t = MapTransport::new(vec![(
            "groups.config.get",
            Ok(json!({
                "agent_group_id": "00000000-0000-0000-0000-000000000001",
                "provider": "anthropic",
                "model": "claude-sonnet",
                "image_tag": null,
                "assistant_name": null,
                "max_messages_per_prompt": null,
                "mcp_servers": {},
                "packages_apt": [],
                "packages_npm": [],
                "updated_at": "2026-01-01T00:00:00Z",
            })),
        )]);
        let out = run_groups_config_edit(
            &json!({
                "id": "00000000-0000-0000-0000-000000000001",
                "dry_run": true,
                "editor_override": "true",
            }),
            &t,
            Caller::Host,
        )
        .await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(out.stdout.contains("no changes"));
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "groups.config.get");
    }

    #[tokio::test]
    async fn groups_config_edit_dry_run_with_changes_prints_diff() {
        let dir = tempfile::tempdir().unwrap();
        let editor_script = write_editor_script(
            dir.path(),
            "#!/bin/sh\nprintf 'provider = \"replaced\"\\nmodel = \"claude-sonnet\"\\n' > \"$1\"\n",
        );
        let t = MapTransport::new(vec![(
            "groups.config.get",
            Ok(json!({
                "agent_group_id": "00000000-0000-0000-0000-000000000002",
                "provider": "anthropic",
                "model": "claude-sonnet",
                "image_tag": null,
                "assistant_name": null,
                "max_messages_per_prompt": null,
                "mcp_servers": {},
                "packages_apt": [],
                "packages_npm": [],
                "updated_at": "2026-01-01T00:00:00Z",
            })),
        )]);
        let out = run_groups_config_edit(
            &json!({
                "id": "00000000-0000-0000-0000-000000000002",
                "dry_run": true,
                "editor_override": editor_script.to_string_lossy(),
            }),
            &t,
            Caller::Host,
        )
        .await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(
            out.stdout.contains("dry-run"),
            "missing dry-run banner; stdout={:?}",
            out.stdout
        );
        assert!(out.stdout.contains("provider"));
        // The transport should have been hit only for the get; no update.
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "groups.config.get");
    }

    #[tokio::test]
    async fn groups_config_edit_commits_scalar_change() {
        let dir = tempfile::tempdir().unwrap();
        let editor_script = write_editor_script(
            dir.path(),
            "#!/bin/sh\nprintf 'provider = \"replaced\"\\nmodel = \"claude-sonnet\"\\n' > \"$1\"\n",
        );
        let t = MapTransport::new(vec![
            (
                "groups.config.get",
                Ok(json!({
                    "agent_group_id": "00000000-0000-0000-0000-000000000003",
                    "provider": "anthropic",
                    "model": "claude-sonnet",
                    "image_tag": null,
                    "assistant_name": null,
                    "max_messages_per_prompt": null,
                    "mcp_servers": {},
                    "packages_apt": [],
                    "packages_npm": [],
                    "updated_at": "2026-01-01T00:00:00Z",
                })),
            ),
            (
                "groups.config.update",
                Ok(
                    json!({"agent_group_id": "00000000-0000-0000-0000-000000000003", "provider":"replaced"}),
                ),
            ),
        ]);
        let out = run_groups_config_edit(
            &json!({
                "id": "00000000-0000-0000-0000-000000000003",
                "dry_run": false,
                "editor_override": editor_script.to_string_lossy(),
            }),
            &t,
            Caller::Host,
        )
        .await;
        assert!(out.stderr.is_empty(), "stderr={:?}", out.stderr);
        assert!(out.stdout.contains("updated"));
        let calls = t.calls.lock().unwrap();
        let cmds: Vec<&str> = calls.iter().map(|(c, _)| c.as_str()).collect();
        assert!(cmds.iter().any(|c| *c == "groups.config.get"));
        assert!(cmds.iter().any(|c| *c == "groups.config.update"));
        // Confirm the update body matches the parsed-out field.
        let upd = calls
            .iter()
            .find(|(c, _)| c == "groups.config.update")
            .unwrap();
        assert_eq!(upd.1["field"], "provider");
        assert_eq!(upd.1["value"], "replaced");
    }

    #[tokio::test]
    async fn groups_config_edit_editor_nonzero_aborts() {
        let t = MapTransport::new(vec![(
            "groups.config.get",
            Ok(json!({
                "agent_group_id": "00000000-0000-0000-0000-000000000004",
                "provider": "anthropic",
                "model": null,
                "image_tag": null,
                "assistant_name": null,
                "max_messages_per_prompt": null,
                "mcp_servers": {},
                "packages_apt": [],
                "packages_npm": [],
                "updated_at": "2026-01-01T00:00:00Z",
            })),
        )]);
        let out = run_groups_config_edit(
            &json!({
                "id": "00000000-0000-0000-0000-000000000004",
                "dry_run": false,
                "editor_override": "false",
            }),
            &t,
            Caller::Host,
        )
        .await;
        assert!(out.stdout.is_empty());
        assert!(
            out.stderr.contains("editor failed"),
            "stderr={:?}",
            out.stderr,
        );
    }

    #[test]
    fn render_config_toml_round_trip_preserves_scalars() {
        let obj = serde_json::Map::from_iter([
            (
                "agent_group_id".into(),
                json!("00000000-0000-0000-0000-000000000005"),
            ),
            ("provider".into(), json!("anthropic")),
            ("model".into(), json!("claude-sonnet")),
            ("image_tag".into(), json!(null)),
            ("assistant_name".into(), json!("Greeter")),
            ("max_messages_per_prompt".into(), json!(16)),
            ("mcp_servers".into(), json!({})),
            ("packages_apt".into(), json!(["curl"])),
            ("packages_npm".into(), json!([])),
            ("updated_at".into(), json!("2026-01-01T00:00:00Z")),
        ]);
        let toml_text = render_config_toml(&obj);
        let parsed = parse_config_toml(&toml_text).expect("toml parses");
        assert_eq!(parsed.get("provider"), Some(&json!("anthropic")));
        assert_eq!(parsed.get("model"), Some(&json!("claude-sonnet")));
        assert_eq!(parsed.get("assistant_name"), Some(&json!("Greeter")));
        assert_eq!(parsed.get("max_messages_per_prompt"), Some(&json!(16)));
        assert_eq!(parsed.get("packages_apt"), Some(&json!(["curl"])));
        // Read-only fields should not appear in the parsed body since
        // they're rendered as comments.
        assert!(parsed.get("agent_group_id").is_none());
        assert!(parsed.get("updated_at").is_none());
    }

    #[test]
    fn dashboard_suggestions_caps_at_three() {
        let groups = json!([]);
        let audit = json!([]);
        let dropped = json!([{"id":"x"}]);
        let sessions = json!([]);
        let s = dashboard_suggestions(&groups, &audit, &dropped, &sessions);
        assert!(s.len() <= 3);
        assert!(!s.is_empty());
    }

    #[test]
    fn host_unreachable_matches_io_kinds() {
        let io = ClientError::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(host_unreachable(&io));
        let to = ClientError::Timeout;
        assert!(!host_unreachable(&to));
    }

    // -- cclaw security audit (end-to-end via run_cli) --
    //
    // A command-keyed transport: maps a command name to a canned response
    // and records the full (command, args) call sequence so a test can
    // assert that `--fix` issued the right *audited* host mutations. Unlike
    // `SequencedTransport` this is order-independent, which matters because
    // the audit fans out a variable number of `groups.config.get` calls.
    struct KeyedTransport {
        responses: Mutex<std::collections::HashMap<String, serde_json::Value>>,
        calls: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl KeyedTransport {
        fn new(pairs: Vec<(&str, serde_json::Value)>) -> Self {
            Self {
                responses: Mutex::new(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls_to(&self, command: &str) -> Vec<serde_json::Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(c, _)| c == command)
                .map(|(_, a)| a.clone())
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl CallTransport for KeyedTransport {
        async fn call(
            &self,
            command: &str,
            args: serde_json::Value,
            _caller: Caller,
        ) -> Result<serde_json::Value, ClientError> {
            self.calls.lock().unwrap().push((command.to_string(), args));
            self.responses
                .lock()
                .unwrap()
                .get(command)
                .cloned()
                .ok_or(ClientError::Timeout)
        }
    }

    /// Posture with a planted open `unknown_sender_policy=open` and an
    /// allow-all egress mode. Loose files come from a throwaway temp dir.
    fn planted_open_responses() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            (
                "egress.status",
                json!({
                    "mode": "allow-all",
                    "groups": [{"agent_group_id": "ag-1", "configured_allow": []}],
                }),
            ),
            ("groups.list", json!([{"id": "ag-1", "name": "demo"}])),
            (
                "groups.config.get",
                json!({"agent_group_id": "ag-1", "tool_profile": "messaging"}),
            ),
            (
                "messaging-groups.list",
                json!([{
                    "id": "mg-1",
                    "name": "lobby",
                    "channel_type": "telegram",
                    "unknown_sender_policy": "open",
                }]),
            ),
            ("messaging-groups.update", json!({"id": "mg-1"})),
            (
                "groups.config.set-egress-allow",
                json!({"egress_allow": []}),
            ),
        ]
    }

    #[tokio::test]
    async fn security_audit_detects_planted_open_policy() {
        let t = KeyedTransport::new(planted_open_responses());
        let tmp = tempfile::tempdir().unwrap();
        let out = run_cli(
            [
                "cclaw",
                "security",
                "audit",
                "--data-dir",
                tmp.path().to_str().unwrap(),
            ],
            &t,
        )
        .await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(
            combined.contains("approvals-open"),
            "missing finding: {combined}"
        );
        assert!(combined.contains("HIGH"));
        assert!(combined.contains("egress-mode"));
        // No --fix -> open policy remains -> failure exit (stderr non-empty).
        assert!(
            !out.stderr.is_empty(),
            "open policy must be a non-zero exit"
        );
        // And it did NOT mutate anything without --fix.
        assert!(t.calls_to("messaging-groups.update").is_empty());
        assert!(t.calls_to("groups.config.set-egress-allow").is_empty());
    }

    #[tokio::test]
    async fn security_audit_fix_remediates_and_audits_open_policy() {
        let t = KeyedTransport::new(planted_open_responses());
        let tmp = tempfile::tempdir().unwrap();
        let out = run_cli(
            [
                "cclaw",
                "security",
                "audit",
                "--fix",
                "--data-dir",
                tmp.path().to_str().unwrap(),
            ],
            &t,
        )
        .await;
        let combined = format!("{}{}", out.stdout, out.stderr);
        // --fix issued the tightening mutation (audited by the host's
        // socket dispatch layer — every HOST_ONLY_COMMANDS call is logged).
        let appr_calls = t.calls_to("messaging-groups.update");
        assert_eq!(
            appr_calls.len(),
            1,
            "exactly one approval tighten: {combined}"
        );
        assert_eq!(appr_calls[0]["id"], "mg-1");
        assert_eq!(
            appr_calls[0]["unknown_sender_policy"], "request_approval",
            "must tighten, never loosen",
        );
        // And it scaffolded the empty allow-list for ag-1.
        let egress_calls = t.calls_to("groups.config.set-egress-allow");
        assert_eq!(egress_calls.len(), 1);
        assert_eq!(egress_calls[0]["id"], "ag-1");
        assert_eq!(egress_calls[0]["allow"][0], security::EGRESS_SCAFFOLD_ENTRY);
        assert!(combined.contains("applied fixes"));
        assert!(
            out.stderr.is_empty(),
            "fix run should succeed: {:?}",
            out.stderr
        );
    }

    #[tokio::test]
    async fn security_audit_never_loosens_a_tight_policy() {
        let t = KeyedTransport::new(vec![
            (
                "egress.status",
                json!({
                    "mode": "deny-default",
                    "groups": [{"agent_group_id": "ag-1", "configured_allow": ["api:443"]}],
                }),
            ),
            ("groups.list", json!([{"id": "ag-1", "name": "demo"}])),
            (
                "groups.config.get",
                json!({"agent_group_id": "ag-1", "tool_profile": "messaging"}),
            ),
            (
                "messaging-groups.list",
                json!([{
                    "id": "mg-1",
                    "name": "lobby",
                    "channel_type": "telegram",
                    "unknown_sender_policy": "request_approval",
                }]),
            ),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let out = run_cli(
            [
                "cclaw",
                "security",
                "audit",
                "--fix",
                "--data-dir",
                tmp.path().to_str().unwrap(),
            ],
            &t,
        )
        .await;
        // Even with --fix, a tight posture must trigger ZERO mutations.
        assert!(t.calls_to("messaging-groups.update").is_empty());
        assert!(t.calls_to("groups.config.set-egress-allow").is_empty());
        assert!(
            out.stdout.contains("no open policies"),
            "stdout={:?}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn security_audit_fix_chmods_loose_session_file_to_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let sess_dir = tmp.path().join("sessions").join("ag").join("sess");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let db = sess_dir.join("inbound.db");
        std::fs::write(&db, b"x").unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();

        let t = KeyedTransport::new(vec![
            (
                "egress.status",
                json!({"mode": "deny-default", "groups": []}),
            ),
            ("groups.list", json!([])),
            ("messaging-groups.list", json!([])),
        ]);
        let out = run_cli(
            [
                "cclaw",
                "security",
                "audit",
                "--fix",
                "--data-dir",
                tmp.path().to_str().unwrap(),
            ],
            &t,
        )
        .await;
        let mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "file should be tightened to 0600");
        assert!(out.stdout.contains("applied fixes"));
    }

    // ---- D1: color + TTY awareness ---------------------------------------

    #[test]
    fn finalise_doctor_plain_palette_has_no_ansi() {
        let checks = vec![
            Check::ok("a", "fine"),
            Check::warn("b", "meh", Some("do the thing")),
            Check::fail("c", "bad", Some("fix the thing")),
        ];
        let out = finalise_doctor(&checks, false, Palette::plain());
        let combined = format!("{}{}", out.stdout, out.stderr);
        assert!(!combined.contains('\u{1b}'));
        assert!(combined.contains("fix: do the thing"));
    }

    #[test]
    fn finalise_doctor_colored_palette_styles_levels_and_fix_hints() {
        let checks = vec![
            Check::ok("a", "fine"),
            Check::warn("b", "meh", Some("do the thing")),
            Check::fail("c", "bad", Some("fix the thing")),
        ];
        let out = finalise_doctor(&checks, false, Palette::new(true));
        let combined = format!("{}{}", out.stdout, out.stderr);
        // Green OK, yellow WARN, red FAIL, cyan fix hints.
        assert!(combined.contains("\u{1b}[32mOK"));
        assert!(combined.contains("\u{1b}[33mWARN"));
        assert!(combined.contains("\u{1b}[31mFAIL"));
        assert!(combined.contains("\u{1b}[36mfix:"));
        // The plain text is still present once escapes are ignored.
        assert!(combined.contains("fix the thing"));
    }

    #[test]
    fn finalise_doctor_json_is_never_styled_even_with_colored_palette() {
        let checks = vec![Check::fail("c", "bad", Some("fix it"))];
        let colored = finalise_doctor(&checks, true, Palette::new(true));
        let plain = finalise_doctor(&checks, true, Palette::plain());
        assert_eq!(colored.stderr, plain.stderr);
        assert_eq!(colored.stdout, plain.stdout);
        assert!(!format!("{}{}", colored.stdout, colored.stderr).contains('\u{1b}'));
    }

    #[test]
    fn dashboard_text_colored_headers_strip_to_plain() {
        let groups = json!([{"id": "ag-1", "name": "g", "agent_provider": "anthropic"}]);
        let empty = json!([]);
        let suggestions = vec!["do a thing".to_string()];
        let plain = render_dashboard_text(
            "/root",
            &groups,
            &empty,
            &empty,
            &empty,
            &empty,
            &empty,
            &suggestions,
            Palette::plain(),
        );
        let colored = render_dashboard_text(
            "/root",
            &groups,
            &empty,
            &empty,
            &empty,
            &empty,
            &empty,
            &suggestions,
            Palette::new(true),
        );
        assert!(!plain.contains('\u{1b}'));
        assert!(colored.contains('\u{1b}'));
        let stripped = colored.replace("\u{1b}[1m", "").replace("\u{1b}[0m", "");
        assert_eq!(stripped, plain);
    }

    #[tokio::test]
    async fn run_cli_unit_tests_are_colorless_by_construction() {
        // `color_enabled` forces the terminal probe to false under
        // cfg(test), so every in-process run_cli invocation stays plain.
        let t = SequencedTransport::new(vec![Ok(json!([{"id": "ag-1", "name": "x"}]))]);
        let out = run_cli(["cclaw", "groups", "list"], &t).await;
        assert!(!out.stdout.contains('\u{1b}'));
        assert!(out.stdout.contains("ID"));
    }

    #[tokio::test]
    async fn remote_error_line_shape_is_unchanged_when_plain() {
        let t = SequencedTransport::new(vec![Err(ClientError::Remote(ErrorPayload::new(
            "boom", "it broke",
        )))]);
        let out = run_cli(["cclaw", "groups", "list"], &t).await;
        assert_eq!(out.stderr, "remote error: it broke (boom)\n");
    }

    // ---- D2: sessions get / sessions tail --------------------------------

    fn sessions_get_payload() -> serde_json::Value {
        json!({
            "id": "0198c0de-0000-7000-8000-000000000001",
            "status": "active",
            "container_status": "running",
            "recent_inbound": [
                {"direction": "in", "seq": 2, "kind": "chat", "status": "completed",
                 "ts": "2026-07-14T10:00:00+00:00", "preview": "hello agent"},
            ],
            "recent_outbound": [
                {"direction": "out", "seq": 3, "kind": "chat", "status": "delivered",
                 "ts": "2026-07-14T10:00:05+00:00", "preview": "hi human"},
            ],
        })
    }

    #[tokio::test]
    async fn sessions_get_renders_session_row_and_message_tables() {
        let t = SequencedTransport::new(vec![Ok(sessions_get_payload())]);
        let out = run_cli(["cclaw", "sessions", "get", "s1"], &t).await;
        // One wire call to sessions.get with the id.
        let calls = t.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sessions.get");
        assert_eq!(calls[0].1, json!({"id": "s1"}));
        drop(calls);
        // Session KV table plus both message sections.
        assert!(out.stdout.contains("container_status"));
        assert!(out.stdout.contains("recent inbound (last 1)"));
        assert!(out.stdout.contains("recent outbound (last 1)"));
        assert!(out.stdout.contains("hello agent"));
        assert!(out.stdout.contains("hi human"));
        // The arrays are not dumped into the KV table as raw JSON.
        assert!(!out.stdout.contains("recent_inbound"));
    }

    #[tokio::test]
    async fn sessions_list_renders_short_parent_indicator() {
        // Phase 4 of docs/plans/agent-to-agent-routing.md: the list
        // table shows a compact PARENT column (8-char prefix of
        // `source_session_id`, empty for roots) instead of the raw
        // 36-char UUID column.
        let rows = json!([
            {
                "id": "0197aaaa-0000-7000-8000-000000000001",
                "status": "active",
                "source_session_id": serde_json::Value::Null,
            },
            {
                "id": "0197bbbb-0000-7000-8000-000000000002",
                "status": "active",
                "source_session_id": "0197aaaa-0000-7000-8000-000000000001",
            },
        ]);
        let t = SequencedTransport::new(vec![Ok(rows)]);
        let out = run_cli(["cclaw", "sessions", "list"], &t).await;
        assert!(out.stdout.contains("PARENT"), "stdout: {}", out.stdout);
        assert!(
            out.stdout.contains("0197aaaa"),
            "child row must show the 8-char parent prefix: {}",
            out.stdout
        );
        assert!(
            !out.stdout.contains("SOURCE_SESSION_ID"),
            "the raw UUID column must be replaced in the table view: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn sessions_list_json_keeps_full_source_session_id() {
        let rows = json!([
            {
                "id": "0197bbbb-0000-7000-8000-000000000002",
                "source_session_id": "0197aaaa-0000-7000-8000-000000000001",
            },
        ]);
        let t = SequencedTransport::new(vec![Ok(rows.clone())]);
        let out = run_cli(["cclaw", "--json", "sessions", "list"], &t).await;
        let parsed: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
        assert_eq!(parsed, rows, "--json must be the untouched wire payload");
    }

    #[tokio::test]
    async fn sessions_get_json_returns_raw_payload() {
        let payload = sessions_get_payload();
        let t = SequencedTransport::new(vec![Ok(payload.clone())]);
        let out = run_cli(["cclaw", "--json", "sessions", "get", "s1"], &t).await;
        let parsed: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
        assert_eq!(parsed, payload, "--json must be the untouched wire payload");
    }

    #[tokio::test]
    async fn sessions_get_without_recents_renders_row_only() {
        // A host that predates D2 (or a foreign-session agent caller)
        // returns just the session row; rendering degrades gracefully.
        let t = SequencedTransport::new(vec![Ok(json!({"id": "x", "status": "active"}))]);
        let out = run_cli(["cclaw", "sessions", "get", "x"], &t).await;
        assert!(out.stdout.contains("active"));
        assert!(!out.stdout.contains("recent inbound"));
        // W3.4 architect-state sections are absent when the host sent none.
        assert!(!out.stdout.contains("declared services"));
        assert!(!out.stdout.contains("services log"));
        assert!(!out.stdout.contains("decisions:"));
    }

    // ---- W3.4: architect-state sections on sessions get -------------------

    fn sessions_get_architect_payload() -> serde_json::Value {
        json!({
            "id": "0198c0de-0000-7000-8000-000000000001",
            "status": "active",
            "container_status": "running",
            "recent_inbound": [],
            "recent_outbound": [],
            "services": [
                "redis-server --daemonize yes --dir /data/redis",
                "pg_ctl -D /data/pg start",
            ],
            "services_log_tail": [
                "[2026-07-28T10:00:00Z] $ redis-server --daemonize yes --dir /data/redis",
                "[2026-07-28T10:00:01Z] status: exit status: 0",
            ],
            "decisions": [
                {"project": "myapp",
                 "tail": ["- 2026-07-28: SQLite over Postgres", "- 2026-07-28: REST over gRPC"]},
            ],
        })
    }

    #[tokio::test]
    async fn sessions_get_renders_architect_state_sections() {
        let t = SequencedTransport::new(vec![Ok(sessions_get_architect_payload())]);
        let out = run_cli(["cclaw", "sessions", "get", "s1"], &t).await;
        // Declared services section with its lines indented.
        assert!(out.stdout.contains("declared services"));
        assert!(
            out.stdout
                .contains("  redis-server --daemonize yes --dir /data/redis\n")
        );
        assert!(out.stdout.contains("  pg_ctl -D /data/pg start\n"));
        // Services log tail section.
        assert!(out.stdout.contains("services log (tail)"));
        assert!(
            out.stdout
                .contains("  [2026-07-28T10:00:01Z] status: exit status: 0\n")
        );
        // Per-project decisions section.
        assert!(out.stdout.contains("decisions: myapp (tail)"));
        assert!(
            out.stdout
                .contains("  - 2026-07-28: SQLite over Postgres\n")
        );
        assert!(out.stdout.contains("  - 2026-07-28: REST over gRPC\n"));
        // The raw arrays are not dumped into the KV table.
        assert!(!out.stdout.contains("services_log_tail"));
        assert!(!out.stdout.contains("\"services\""));
    }

    #[tokio::test]
    async fn sessions_get_renders_empty_services_as_none() {
        // File exists but declares nothing: the host sends an empty
        // array and the section renders as (none).
        let t = SequencedTransport::new(vec![Ok(json!({
            "id": "x",
            "status": "active",
            "services": [],
        }))]);
        let out = run_cli(["cclaw", "sessions", "get", "x"], &t).await;
        assert!(out.stdout.contains("declared services"));
        assert!(out.stdout.contains("  (none)\n"));
    }

    #[tokio::test]
    async fn sessions_get_json_keeps_architect_state_untouched() {
        let payload = sessions_get_architect_payload();
        let t = SequencedTransport::new(vec![Ok(payload.clone())]);
        let out = run_cli(["cclaw", "--json", "sessions", "get", "s1"], &t).await;
        let parsed: serde_json::Value = serde_json::from_str(out.stdout.trim()).unwrap();
        assert_eq!(parsed, payload, "--json must be the untouched wire payload");
    }

    fn tail_payload() -> serde_json::Value {
        json!({
            "session_id": "s1",
            "rows": [
                {"direction": "in", "seq": 2, "kind": "chat", "status": "completed",
                 "ts": "2026-07-14T10:00:00+00:00", "preview": "hello agent"},
                {"direction": "out", "seq": 1, "kind": "breadcrumb", "status": "pending",
                 "ts": "2026-07-14T10:00:02+00:00", "preview": "[shell] cargo check"},
                {"direction": "out", "seq": 3, "kind": "chat", "status": "delivered",
                 "ts": "2026-07-14T10:00:05+00:00", "preview": "hi human"},
            ],
            "last_in_seq": 2,
            "last_out_seq": 3,
        })
    }

    #[tokio::test]
    async fn sessions_tail_one_shot_prints_ordered_rows_with_markers() {
        let t = SequencedTransport::new(vec![Ok(tail_payload())]);
        let out = run_cli(["cclaw", "sessions", "tail", "s1"], &t).await;
        let lines: Vec<&str> = out.stdout.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("<- "));
        assert!(lines[0].contains("hello agent"));
        assert!(lines[1].contains("-- "));
        assert!(lines[1].contains("[shell] cargo check"));
        assert!(lines[2].contains("-> "));
        assert!(lines[2].contains("hi human"));
        // Time order is preserved as delivered by the host.
        assert!(lines[0].starts_with("2026-07-14T10:00:00"));
        assert!(lines[2].starts_with("2026-07-14T10:00:05"));
    }

    #[tokio::test]
    async fn sessions_tail_empty_says_so() {
        let t = SequencedTransport::new(vec![Ok(
            json!({"session_id": "s1", "rows": [], "last_in_seq": 0, "last_out_seq": 0}),
        )]);
        let out = run_cli(["cclaw", "sessions", "tail", "s1"], &t).await;
        assert_eq!(out.stdout, "(no message rows yet)\n");
    }

    #[tokio::test]
    async fn sessions_tail_follow_rejects_json() {
        let t = SequencedTransport::new(vec![]);
        let out = run_cli(
            ["cclaw", "--json", "sessions", "tail", "s1", "--follow"],
            &t,
        )
        .await;
        assert!(out.stderr.contains("--follow"));
        assert!(t.calls.lock().unwrap().is_empty(), "no wire call issued");
    }

    #[tokio::test]
    async fn sessions_tail_remote_error_is_surfaced() {
        let t = SequencedTransport::new(vec![Err(ClientError::Remote(ErrorPayload::new(
            "not_found",
            "no such session",
        )))]);
        let out = run_cli(["cclaw", "sessions", "tail", "s1"], &t).await;
        assert!(
            out.stderr
                .contains("remote error: no such session (not_found)")
        );
    }

    #[test]
    fn tail_marker_covers_directions_and_status_kinds() {
        assert_eq!(tail_marker("in", "chat"), "<-");
        assert_eq!(tail_marker("out", "chat"), "->");
        assert_eq!(tail_marker("out", "breadcrumb"), "--");
        assert_eq!(tail_marker("in", "todo_list"), "--");
        assert_eq!(tail_marker("out", "diff"), "--");
        assert_eq!(tail_marker("out", "card"), "->");
    }

    #[test]
    fn format_tail_row_is_single_line() {
        let row = json!({"direction": "in", "seq": 2, "kind": "chat", "status": "pending",
                         "ts": "t", "preview": "p"});
        let line = format_tail_row(&row);
        assert!(!line.contains('\n'));
        assert!(line.contains("<- "));
        assert!(line.ends_with('p'));
    }
}
