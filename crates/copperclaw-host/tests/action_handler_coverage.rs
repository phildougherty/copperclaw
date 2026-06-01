//! Structural meta-test: every system action name the runner can emit
//! MUST be handled host-side, either by an inline handler in
//! `DeliveryService` or by a `DeliveryActionHandler` registered against
//! the dispatcher by one of the built-in modules.
//!
//! Today's biggest bug class was silent name-mismatches between runner
//! emit sites and module register sites. This test makes such a mismatch
//! impossible to land without a CI failure.
//!
//! Four tests live in this file:
//!
//! 1. `every_runner_emit_has_a_host_handler` — the actual coverage gate.
//! 2. `runner_emit_set_matches_source` — keeps the hard-coded emit set
//!    in (1) honest by re-deriving it from runner source.
//! 3. `host_handle_set_matches_inline_arms` — keeps the hard-coded
//!    inline-handler set in (1) honest by re-deriving it from delivery
//!    source.
//! 4. `every_module_action_name_is_lowercase_snake` — quality gate on
//!    the shape of every registered name.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use copperclaw_modules::context::MockModuleContext;
use copperclaw_modules::{
    AgentToAgentModule, ApprovalsModule, CreateAgentModule, InteractiveModule, Module,
    ModuleContext, MountSecurityModule, PermissionsModule, SchedulingModule, SelfModModule,
    TypingConfig, TypingModule, create_agent_always_allow,
};

/// Action names the runner currently emits as `MessageKind::System`
/// outbound rows. The host MUST have a handler for every one of these
/// or the row will be silently marked `delivered=ok` with no effect.
///
/// Source of truth (regex-checked by test `runner_emit_set_matches_source`):
/// `grep -nE 'serde_json::json!\(\{ ?"[a-z_]+":' crates/copperclaw-runner/src/tools.rs`
/// plus the `usage_report` emitter in `crates/copperclaw-runner/src/run.rs`.
fn runner_emit_set() -> HashSet<&'static str> {
    [
        // Per-turn provider usage rollup written by `emit_usage_report`
        // in run.rs at the end of every turn.
        "usage_report",
        // Message lifecycle actions (apply_edit_message, apply_add_reaction).
        "edit",
        "reaction",
        // Interactive UI (apply_ask_question). `send_card` was removed
        // from this list in wave 2 of the cards rollout: `apply_send_card`
        // now writes a `MessageKind::Card` row (NOT a System action) and
        // goes through the delivery service's `dispatch_card` path, not
        // the module action registry.
        "ask_user_question",
        // Agent-to-agent fan-out (apply_create_agent).
        "create_agent",
        // Self-modification (apply_install_packages, apply_add_mcp_server).
        "install_packages",
        "add_mcp_server",
        // Scheduling — every op (create / list / cancel / pause / resume /
        // update) emits the same top-level "schedule" key with an inner `op`.
        "schedule",
        // Tool-progress chip finalisation. `emit_breadcrumb_finish` in
        // tools.rs writes a System row carrying this action; the host's
        // delivery service intercepts it inline (see
        // `inline_handler_set`) and drives an in-place edit of the
        // prior chip via `deliver_breadcrumb(..., existing_message_id)`.
        "update_breadcrumb",
    ]
    .into_iter()
    .collect()
}

/// Action names that `DeliveryService::handle_system` intercepts directly
/// before consulting the module-registered handler map. These are kept in
/// sync with the actual `if action.name == "..."` arms by test
/// `host_handle_set_matches_inline_arms`.
fn inline_handler_set() -> HashSet<&'static str> {
    [
        "usage_report",
        "install_packages",
        "add_mcp_server",
        // `update_breadcrumb` is the finalisation half of the runner's
        // tool-progress chip pipeline. Intercepted inline (rather than
        // via the module registry) so the host can resolve the prior
        // chip's platform message id and drive an in-place edit via
        // the adapter's `deliver_breadcrumb` hook.
        "update_breadcrumb",
        // `edit` and `reaction` go through the typed adapter API
        // (`maybe_handle_edit_or_reaction`). They also fall through to
        // the module-registered handler when the adapter is `Unsupported`.
        "edit",
        "reaction",
    ]
    .into_iter()
    .collect()
}

/// Install the same module set that `boot::install_modules` does, against a
/// `MockModuleContext` that records every `register_delivery_action(name)`
/// call.
///
/// Modules that need a real `CentralDb` to function still install cleanly
/// against the mock — `register_delivery_action` only takes a name and an
/// `Arc<dyn DeliveryActionHandler>`; the mock just stores them.
///
/// This mirrors the production module list exactly. If `install_modules`
/// later forgets to install a module that registers a runner-targeted
/// action, this helper will mirror that omission and the coverage test
/// will fail loudly.
async fn install_built_in_modules_mock(ctx: Arc<MockModuleContext>) {
    // Order mirrors `crates/copperclaw-host/src/boot.rs::install_modules`.
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(TypingModule::new(TypingConfig::default())),
        Box::new(MountSecurityModule::new()),
        Box::new(PermissionsModule::deny_all()),
        Box::new(ApprovalsModule::new()),
        Box::new(InteractiveModule::default()),
        Box::new(SchedulingModule::default()),
        Box::new(AgentToAgentModule),
        // CreateAgentModule needs a CentralDb + data_root. Use an
        // in-memory DB and a tempdir to keep the mock-install pure.
        Box::new(CreateAgentModule::new(
            copperclaw_db::central::CentralDb::open_in_memory().unwrap(),
            std::env::temp_dir().join(format!(
                "copperclaw-action-coverage-{}",
                uuid::Uuid::new_v4()
            )),
            create_agent_always_allow(),
        )),
        Box::new(SelfModModule),
    ];
    for m in modules {
        let c: Arc<dyn ModuleContext> = Arc::clone(&ctx) as Arc<dyn ModuleContext>;
        m.install(c)
            .await
            .expect("test module install should not fail against MockModuleContext");
    }
}

#[tokio::test]
async fn every_runner_emit_has_a_host_handler() {
    let runner_emits = runner_emit_set();

    let mut host_handles: HashSet<String> = HashSet::new();
    for n in inline_handler_set() {
        host_handles.insert(n.to_string());
    }
    let ctx = MockModuleContext::new();
    install_built_in_modules_mock(Arc::clone(&ctx)).await;
    for name in ctx.delivery_actions() {
        host_handles.insert(name);
    }

    let unhandled: Vec<&str> = runner_emits
        .iter()
        .filter(|n| !host_handles.contains(**n))
        .copied()
        .collect();

    assert!(
        unhandled.is_empty(),
        "RUNNER EMITS ACTION NAMES WITH NO HOST HANDLER: {unhandled:?}\n\
         \n\
         This is the bug class that caused today's silent-inert subsystems\n\
         (`ask_question` vs `ask_user_question`, `card` vs `send_card`,\n\
         SchedulingModule no-op install, AgentToAgentModule registering\n\
         nothing). Each of those compiled, had passing unit tests on both\n\
         sides, and shipped to production undetected.\n\
         \n\
         Fix: either register a module handler with the matching name in\n\
         `crates/copperclaw-modules/src/<module>.rs::install`, or add the\n\
         action to `DeliveryService::handle_system`'s inline arms in\n\
         `crates/copperclaw-host-delivery/src/service.rs`. If the missing\n\
         action is registered by a module that exists but is not added to\n\
         `boot::install_modules` in `crates/copperclaw-host/src/boot.rs`,\n\
         wire it up there.\n\
         \n\
         Source of truth for runner emits:\n\
         `grep -nE 'serde_json::json!\\(\\{{ ?\"[a-z_]+\":' \\\n\
              crates/copperclaw-runner/src/tools.rs`\n\
         plus the `usage_report` emitter in\n\
         `crates/copperclaw-runner/src/run.rs::emit_usage_report`."
    );
}

/// Re-derive the runner emit set from source and assert it matches the
/// hard-coded list in `runner_emit_set()`. Keeps that list from drifting
/// silently when a new `apply_*` function gets added.
#[test]
fn runner_emit_set_matches_source() {
    let tools_src = std::fs::read_to_string(runner_path("tools.rs"))
        .expect("read crates/copperclaw-runner/src/tools.rs");
    // `run.rs` was split into a directory module (`run/mod.rs` plus
    // focused sub-files) in the runner refactor. `emit_usage_report`
    // stayed in `mod.rs` alongside the main loop because it locks the
    // outbound `Connection`; the regex scan only needs the file that
    // contains its body.
    let run_src = std::fs::read_to_string(runner_path("run/mod.rs"))
        .expect("read crates/copperclaw-runner/src/run/mod.rs");

    // Match `serde_json::json!({ "<name>": ...` where <name> is the
    // first key inside the top-level object. This is the pattern every
    // `apply_*` helper uses to construct its `MessageKind::System` payload.
    //
    // We restrict to the body of `fn apply_*` (and the body of
    // `emit_usage_report` in run.rs) so that doc-example snippets in
    // module headers don't pollute the set.
    let re = regex::Regex::new(r#"serde_json::json!\(\{\s*"([a-z][a-z0-9_]*)"\s*:"#)
        .expect("compile regex");

    let mut derived: HashSet<String> = HashSet::new();
    for src in [&tools_src, &run_src] {
        for fn_body in extract_fn_bodies(
            src,
            &[
                "fn apply_",
                "fn emit_usage_report",
                // Trailing `(` pins this to the function definition,
                // not the unit-test names like
                // `fn emit_breadcrumb_finish_writes_update_system_row`
                // that would otherwise share the prefix.
                "fn emit_breadcrumb_finish(",
                // The `update_breadcrumb` System action is constructed in
                // this shared helper (used by both the legacy finish path
                // and the rolling-activity start/finish emits).
                "fn insert_update_breadcrumb_row(",
            ],
        ) {
            for cap in re.captures_iter(&fn_body) {
                derived.insert(cap[1].to_string());
            }
        }
    }
    // Strip noise: nested keys inside payload values we don't care about.
    // Currently nothing to strip — the regex only matches the outer
    // `json!({ "first_key": ... })` form. If new noise appears, prune here.

    let hard_coded: HashSet<String> = runner_emit_set().iter().map(|s| (*s).to_string()).collect();

    let missing_from_hardcoded: Vec<&String> = derived.difference(&hard_coded).collect();
    let missing_from_source: Vec<&String> = hard_coded.difference(&derived).collect();

    assert!(
        missing_from_hardcoded.is_empty() && missing_from_source.is_empty(),
        "runner emit set drifted from source.\n\
         \n\
         In source but not in `runner_emit_set()`: {missing_from_hardcoded:?}\n\
         (a new `apply_*` started emitting this — add it to the hard-coded list).\n\
         \n\
         In `runner_emit_set()` but not in source: {missing_from_source:?}\n\
         (an `apply_*` was deleted or renamed — remove the stale entry).\n\
         \n\
         Source files scanned:\n\
         - crates/copperclaw-runner/src/tools.rs (every `fn apply_*` body)\n\
         - crates/copperclaw-runner/src/run.rs (`fn emit_usage_report` body)"
    );
}

/// Re-derive the inline-handler set from `DeliveryService::handle_system`
/// in delivery service source. Keeps the hard-coded list in
/// `inline_handler_set()` honest.
#[test]
fn host_handle_set_matches_inline_arms() {
    let service_src = std::fs::read_to_string(delivery_path("service.rs"))
        .expect("read crates/copperclaw-host-delivery/src/service.rs");

    // Top-of-function arms: `if action.name == "<name>"`.
    let if_re =
        regex::Regex::new(r#"if\s+action\.name\s*==\s*"([a-z_]+)""#).expect("compile regex");
    // The edit/reaction path uses `action.name != "edit" && action.name != "reaction"`.
    let neq_re = regex::Regex::new(r#"action\.name\s*!=\s*"([a-z_]+)""#).expect("compile regex");
    // The action-by-name match in `try_action_via_adapter` —
    // `"edit" => { ... } "reaction" => { ... }`.
    let typed_re = regex::Regex::new(r#"^\s*"(edit|reaction)"\s*=>\s*\{"#).expect("compile regex");

    let mut derived: HashSet<String> = HashSet::new();
    for cap in if_re.captures_iter(&service_src) {
        derived.insert(cap[1].to_string());
    }
    for cap in neq_re.captures_iter(&service_src) {
        derived.insert(cap[1].to_string());
    }
    for line in service_src.lines() {
        if let Some(cap) = typed_re.captures(line) {
            derived.insert(cap[1].to_string());
        }
    }

    let hard_coded: HashSet<String> = inline_handler_set()
        .iter()
        .map(|s| (*s).to_string())
        .collect();

    let missing_from_hardcoded: Vec<&String> = derived.difference(&hard_coded).collect();
    let missing_from_source: Vec<&String> = hard_coded.difference(&derived).collect();

    assert!(
        missing_from_hardcoded.is_empty() && missing_from_source.is_empty(),
        "inline-handler set drifted from `DeliveryService::handle_system`.\n\
         \n\
         In source but not in `inline_handler_set()`: {missing_from_hardcoded:?}\n\
         (a new `if action.name == \"...\"` arm landed — add to hard-coded list).\n\
         \n\
         In `inline_handler_set()` but not in source: {missing_from_source:?}\n\
         (an inline arm was removed — drop the stale entry).\n\
         \n\
         Source file scanned:\n\
         - crates/copperclaw-host-delivery/src/service.rs"
    );
}

/// Every action name a built-in module registers must be lowercase snake
/// case. Some platforms have been bitten by camelCase or kebab-case
/// names that the runner's emitter can't reach because the JSON key would
/// have to be escaped differently.
#[tokio::test]
async fn every_module_action_name_is_lowercase_snake() {
    let ctx = MockModuleContext::new();
    install_built_in_modules_mock(Arc::clone(&ctx)).await;

    let re = regex::Regex::new(r"^[a-z][a-z0-9_]*$").expect("compile regex");
    let bad: Vec<String> = ctx
        .delivery_actions()
        .into_iter()
        .filter(|n| !re.is_match(n))
        .collect();
    assert!(
        bad.is_empty(),
        "module-registered delivery action names must be lowercase_snake: {bad:?}"
    );
}

// --------------------------------------------------------------------------
// Helpers
// --------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at `crates/copperclaw-host/`; workspace
    // root is two `parent()` calls up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root above copperclaw-host crate dir")
        .to_path_buf()
}

fn runner_path(file: &str) -> PathBuf {
    workspace_root()
        .join("crates")
        .join("copperclaw-runner")
        .join("src")
        .join(file)
}

fn delivery_path(file: &str) -> PathBuf {
    workspace_root()
        .join("crates")
        .join("copperclaw-host-delivery")
        .join("src")
        .join(file)
}

/// Extract the body of every function whose declaration starts with one
/// of `prefixes` (e.g. `["fn apply_"]`). Returns the body text between
/// the opening `{` and its matching closing `}`. Handles nested braces.
fn extract_fn_bodies(src: &str, prefixes: &[&str]) -> Vec<String> {
    let mut bodies = Vec::new();
    let bytes = src.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        // Find the next function-prefix start.
        let Some((start, prefix_len)) = find_next_prefix(src, idx, prefixes) else {
            break;
        };
        // Find the opening brace after the function signature.
        let Some(open) = find_unquoted_byte(src, start + prefix_len, b'{') else {
            break;
        };
        // Find the matching close brace.
        let Some(close) = find_matching_brace(src, open) else {
            break;
        };
        bodies.push(src[open + 1..close].to_string());
        idx = close + 1;
    }
    bodies
}

fn find_next_prefix(src: &str, from: usize, prefixes: &[&str]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for p in prefixes {
        if let Some(rel) = src[from..].find(p) {
            let abs = from + rel;
            if best.is_none_or(|(b, _)| abs < b) {
                best = Some((abs, p.len()));
            }
        }
    }
    best
}

/// Find the next occurrence of `target` byte starting at `from`, ignoring
/// any occurrence inside a `"..."` string literal or a `//` / `/* */`
/// comment. Returns the absolute byte index.
fn find_unquoted_byte(src: &str, from: usize, target: u8) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut i = from;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        // Skip line comments.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Skip block comments.
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if b == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if b == target {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Given the index of an opening `{`, find the matching `}` accounting
/// for nested braces, string literals, and comments.
fn find_matching_brace(src: &str, open: usize) -> Option<usize> {
    let bytes = src.as_bytes();
    let mut depth: i32 = 0;
    let mut i = open;
    let mut in_string = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if b == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}
