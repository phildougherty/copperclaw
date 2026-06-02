//! System-prompt assembly, project briefing, skills catalogue, and memory marker.

use super::config::SkillsMode;
use copperclaw_db::tables::container_configs;
use copperclaw_types::AgentGroupId;
use std::path::Path;
use tracing::warn;

/// Filename for an optional operator-supplied project briefing read at
/// spawn time and prepended to the agent's system prompt. The
/// convention mirrors Claude Code's `CLAUDE.md`: anything the operator
/// wants the agent to know about *this* deployment that wouldn't be
/// obvious from the inbound message alone (house style, identity, the
/// shape of the workload). Two locations are checked, both optional:
/// the session dir (`/data/COPPERCLAW.md` from the container's
/// perspective) and the per-group override directory
/// (`<groups_dir>/<agent_group_id>/COPPERCLAW.md`).
pub const PROJECT_BRIEFING_FILENAME: &str = "COPPERCLAW.md";

/// Filename for the per-session skills catalogue. Written into the
/// session dir alongside `runner.json` when `skills_mode = Callable` so
/// the runner's `load_skill` MCP tool can pull skill bodies on demand
/// instead of pre-inlining every body into the system prompt. From the
/// container's view this lands at `/data/skills.json`.
pub const SKILLS_CATALOGUE_FILENAME: &str = "skills.json";

/// Universal preamble prepended to every agent's system prompt before
/// the environment block, project briefing, and skill catalogue. The
/// content is deliberately mode-agnostic — it describes how to *be* an
/// Copperclaw agent (read carefully, act with care, prefer dedicated
/// tools, reply concisely) without assuming the agent is doing any
/// particular kind of work. Coding, support, scheduling, etc. are all
/// served by the same disciplines; specialised guidance lives in opt-in
/// skills.
pub const BASE_PREAMBLE: &str = "\
You are a Copperclaw agent — a self-hosted assistant that reaches \
people through channel adapters from inside a per-session Linux \
container. Your capabilities are the skills catalogued below; read \
the catalogue before choosing a tool.

# How to work

Read the inbound before acting. Take one tool call at a time and read \
each result before the next — never assert what a tool call didn't \
confirm. If a request is genuinely ambiguous, ask one focused question \
instead of guessing.

Don't introduce yourself or list what you can do unless asked — \
\"who are you?\" / \"what is Copperclaw?\" go through the `identity` \
skill; for any other opening message that's a task, just do it. Skip \
openers and closers (\"Great question!\", \"Let me know if…\"): give \
the thing asked for, no preamble or postamble.

# Planning multi-step work (mandatory)

Anything past two tool calls — build X, research Y and report, set up \
Z, anything with several deliverables or that branches on what you \
find — starts with one `todo_add` per step, before any work. Mark each \
`in_progress` when you start it and `completed` when it's done; \
`todo_list` to reorient. A single-shot answer needs no todos. If \
you're several calls deep and realise it's bigger than you thought, \
stop, `todo_add` the rest, then continue. Skipping the plan is how \
agents lose the thread.

# Keep going until it's done

Do the work in this turn's tool loop — don't stop early. Never announce \
what you're about to do and then end your reply (\"I'll start now\", \
\"Let me build X\"): nothing runs after the reply ends, so a promise to \
continue is just the task dying on the spot. After each tool result, \
take the next concrete step toward the goal; don't pause to narrate or \
to ask whether to carry on. A plan is not progress — the moment you \
finish `todo_add`, do step one, then the next, until every todo is \
`completed`. Stop only when the task is actually done and verified, or \
you hit a blocker you genuinely can't get past — and then say exactly \
what is blocking you, don't go quiet.

# Acting with care

Match boldness to reversibility. Reading, searching, asking — go \
ahead. Editing a file, sending a reply, spawning a sibling — fine when \
asked; pause if it's hard to undo. Deleting, rewriting history, or \
changing configuration the user didn't mention — confirm first. \
Unexpected state (a file, branch, or lock you didn't make) is usually \
the user's work in progress; investigate before overwriting.

# Picking tools

Prefer the dedicated tool over `shell` — `grep` and `glob`, not a \
shelled-out `find`. Independent checks go in one turn as parallel \
calls; sequence only when one needs another's output.

# Don't fabricate

The skill catalogue is the authoritative list of what exists — never \
invent a tool, sub-agent, or behaviour. Unsure a capability exists? \
Say so and `load_skill` the relevant area rather than describing \
something fictional and walking it back later.

- Recurring or future-dated work (\"every N minutes\", \"daily \
  digest\") is `schedule_task` with a cron `recurrence` — the host \
  re-invokes you when it fires; there is no persistent loop or \
  background monitor. `load_skill(\"schedule-task\")` first. Sub-agents \
  that report back: the `create-agent` skill.
- `install_packages` and `add_mcp_server` change the image for your \
  NEXT session, not the container you're in now — the tool won't \
  appear this turn and there is nothing to wait for. Need it this \
  session? Install it into `/data` yourself (download the \
  binary/tarball; in-container `apt` often has no repo access).
- Don't fake completion. Before marking a code todo `completed`, \
  confirm the files exist and hold the work (`read_file` / `glob` / \
  `git_status`). Docs, scaffolding, or compose/build files pointing at \
  code or directories you never created don't count — build the thing, \
  then call it done. If you told the user \"done\", it must survive \
  `ls` and `git log`.

# Sub-agents: explore vs create_agent

Both can read your code. For a QUICK focused lookup use `explore` (runs in \
your container, sees your live workspace, bounded + cheap). For \
SUBSTANTIVE parallel work use `create_agent`: each sibling runs in its own \
container. If the project you're CURRENTLY IN (your shell's working dir) is \
a GIT REPO, the sibling gets a WRITABLE worktree of THAT repo at \
`/workspace` on branch `sib/<id>` — it can edit AND commit there without \
touching your files; afterwards you review and merge its branch from inside \
that project (`git diff main..sib/<id>`, `git merge sib/<id>`, then \
`git worktree remove .copperclaw/wt/<id>` to tidy up). So `cd` into the \
project before spawning builders — and keep every project a git repo (one \
repo per project dir; `git init` new ones at creation). If you're not in a \
repo, your whole workspace is mounted READ-ONLY at `/parent` (review/audit \
only). Tell each sibling where to work in its `instructions` (e.g. \
'implement X under /workspace and commit' or 'review the code under /parent').

Children spawned via `create_agent` report into your `messages_in` \
(the host routes them; they do NOT post to the user). Wait for all N \
to arrive before replying, don't narrate progress, don't tell the user \
the children will post (they won't), and send ONE reply that \
synthesises across them — never relay a child's raw output verbatim.

# Replying

Be concise: don't restate the request or recap what you did at the \
end. One or two sentences is usually enough; add a code block, \
command, or link when it helps and skip the prose around it. Never use \
emojis unless the user explicitly asks.
";

/// Filename of the per-session marker dropped when the per-group memory
/// mount could not be configured. The agent reads
/// `/data/memory/UNAVAILABLE.md` and learns the writes it makes here are
/// session-local rather than persistent.
pub const MEMORY_UNAVAILABLE_FILENAME: &str = "UNAVAILABLE.md";

/// Build the environment block: a short, structured snapshot of the
/// agent's context that an operator would otherwise have to teach via
/// skills (today's date, which session is running, the working directory
/// inside the container, the assistant's display name when set). Mirrors
/// the equivalent block Claude Code injects at the top of its system
/// prompt.
pub(crate) fn environment_block(
    session_id: copperclaw_types::SessionId,
    agent_group_id: AgentGroupId,
    now: chrono::DateTime<chrono::Utc>,
    assistant_name: Option<&str>,
) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("\n# Environment\n\n");
    out.push_str(&format!("Today is {}.\n", now.format("%Y-%m-%d")));
    out.push_str(&format!("Session id: {}\n", session_id.as_uuid()));
    out.push_str(&format!("Agent group id: {}\n", agent_group_id.as_uuid()));
    out.push_str(&format!(
        "Working directory: {} (per-session bind mount; this is where \
         inbound.db, outbound.db, and your runner config live)\n",
        super::spawn::CONTAINER_SESSION_DIR,
    ));
    if let Some(name) = assistant_name {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            out.push_str(&format!("Assistant name: {trimmed}\n"));
        }
    }
    out
}

/// Read the optional project briefing from disk. Two sources, both
/// optional:
///
/// 1. The session dir (`<session_root>/COPPERCLAW.md`) — operator-supplied
///    per-session context, e.g. dropped in by a wrapper that materialised
///    a specific workload before the runner booted.
/// 2. The per-group override (`<groups_dir>/<agent_group_id>/COPPERCLAW.md`)
///    — operator-supplied per-group context that applies to every session
///    of this group.
///
/// Returns `None` if neither file exists. Read errors are logged and the
/// briefing is dropped — a missing or malformed briefing must not block
/// spawn.
pub(crate) fn read_project_briefing(
    session_root: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
) -> Option<String> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut diagnostics: Vec<String> = Vec::new();

    if let Some(dir) = groups_dir {
        let path = dir
            .join(agent_group_id.as_uuid().to_string())
            .join(PROJECT_BRIEFING_FILENAME);
        match std::fs::read_to_string(&path) {
            Ok(body) if !body.trim().is_empty() => {
                sections.push((format!("group: {}", path.display()), body));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                warn!(?err, path = %path.display(), "could not read group briefing");
                diagnostics.push(format!(
                    "The operator-supplied group briefing at {} could not be read ({}); \
                     the agent has no context from that file for this session.",
                    path.display(),
                    err.kind(),
                ));
            }
        }
    }

    if let Some(root) = session_root {
        let path = root.join(PROJECT_BRIEFING_FILENAME);
        match std::fs::read_to_string(&path) {
            Ok(body) if !body.trim().is_empty() => {
                sections.push((format!("session: {}", path.display()), body));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                warn!(?err, path = %path.display(), "could not read session briefing");
                diagnostics.push(format!(
                    "The operator-supplied session briefing at {} could not be read ({}); \
                     the agent has no context from that file for this session.",
                    path.display(),
                    err.kind(),
                ));
            }
        }
    }

    if sections.is_empty() && diagnostics.is_empty() {
        return None;
    }

    let mut out = String::with_capacity(2 * 1024);
    out.push_str("\n# Project briefing\n\n");
    if !sections.is_empty() {
        out.push_str(
            "The operator supplied the following briefing(s); treat them as \
             authoritative context for this deployment.\n",
        );
        for (source, body) in sections {
            out.push_str(&format!(
                "\n<briefing source=\"{}\">\n",
                escape_attr(&source)
            ));
            out.push_str(body.trim_end_matches('\n'));
            out.push_str("\n</briefing>\n");
        }
    }
    if !diagnostics.is_empty() {
        out.push_str("\n## Briefing diagnostics\n\n");
        for line in diagnostics {
            out.push_str(&format!("Note: {line}\n"));
        }
    }
    Some(out)
}

/// Top-level system-prompt assembler: stitches the universal preamble,
/// the environment block, an optional operator-supplied project briefing,
/// and the skill catalogue into a single string the runner writes into
/// `runner.json` and the provider sends as the `system` message.
///
/// Each piece is independent: a deployment without a skills directory
/// still gets the preamble + environment; a deployment without an
/// `COPPERCLAW.md` still gets the rest. The order is fixed (preamble →
/// environment → briefing → skills) so that operator briefings can refer
/// back to the environment block without forward-references.
///
/// `skills_mode` controls whether full skill bodies are inlined into the
/// prompt or just a name/description index is emitted (with bodies
/// reachable on demand via the `load_skill` MCP tool).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_system_prompt(
    skills_dir: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
    selector: &copperclaw_skills::SkillsSelector,
    session_root: Option<&Path>,
    session_id: copperclaw_types::SessionId,
    now: chrono::DateTime<chrono::Utc>,
    assistant_name: Option<&str>,
    skills_mode: SkillsMode,
) -> String {
    assemble_system_prompt_with_catalogue(
        skills_dir,
        groups_dir,
        agent_group_id,
        selector,
        session_root,
        session_id,
        now,
        assistant_name,
        skills_mode,
        None,
        &[],
    )
}

/// Like [`assemble_system_prompt`] but accepts a pre-built Callable-mode
/// catalogue so the host can render the prompt index off the same data
/// it just wrote to `skills.json`. Pass `None` to let the assembler
/// build whatever it needs internally.
///
/// `exclude_names` is forwarded to [`build_skill_system_prompt`] so the
/// Inline-mode fallback (no prebuilt catalogue) honours the same filter
/// as the Callable path. When a prebuilt catalogue is supplied the
/// caller is expected to have already applied any filtering.
#[allow(clippy::too_many_arguments)]
pub(crate) fn assemble_system_prompt_with_catalogue(
    skills_dir: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
    selector: &copperclaw_skills::SkillsSelector,
    session_root: Option<&Path>,
    session_id: copperclaw_types::SessionId,
    now: chrono::DateTime<chrono::Utc>,
    assistant_name: Option<&str>,
    skills_mode: SkillsMode,
    prebuilt_catalogue: Option<&[SkillCatalogueEntry]>,
    exclude_names: &[&str],
) -> String {
    let mut out = String::with_capacity(16 * 1024);
    out.push_str(BASE_PREAMBLE);
    out.push_str(&environment_block(
        session_id,
        agent_group_id,
        now,
        assistant_name,
    ));
    if let Some(brief) = read_project_briefing(session_root, groups_dir, agent_group_id) {
        out.push_str(&brief);
    }
    let skills_section = match (skills_mode, prebuilt_catalogue) {
        (SkillsMode::Callable, Some(cat)) if !cat.is_empty() => render_callable_skill_index(cat),
        (SkillsMode::Callable, Some(_)) => String::new(),
        _ => build_skill_system_prompt(
            skills_dir,
            groups_dir,
            agent_group_id,
            selector,
            skills_mode,
            exclude_names,
        ),
    };
    if !skills_section.is_empty() {
        out.push('\n');
        out.push_str(&skills_section);
    }
    out
}

/// Per-skill record written into `skills.json` for the runner-side
/// `load_skill` MCP tool to consume. Kept simple (no extra metadata)
/// so the schema is forward-compatible.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct SkillCatalogueEntry {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) body: String,
}

/// Single source of truth for which skills make it into a Callable-mode
/// spawn: scans the registry, reads each selected skill's body, and
/// drops any that fail to load. The returned vector is the union used
/// by both the prompt index and the `skills.json` catalogue write, so
/// the two cannot disagree about which skills exist.
///
/// `exclude_names` is a post-resolution filter applied after the
/// registry's selector logic: any skill whose `name` matches is dropped.
/// Used by the `coding_enabled = false` path to cap `SkillsSelector::All`
/// without disturbing explicit selector semantics.
pub(crate) fn select_callable_skills(
    skills_dir: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
    selector: &copperclaw_skills::SkillsSelector,
    exclude_names: &[&str],
) -> Vec<SkillCatalogueEntry> {
    let Some(global) = skills_dir else {
        return Vec::new();
    };
    let group_override = groups_dir
        .map(|root| {
            root.join(agent_group_id.as_uuid().to_string())
                .join("skills")
        })
        .filter(|p| p.is_dir())
        .map(|p| (agent_group_id, p));
    let registry = match copperclaw_skills::SkillRegistry::scan(
        global,
        group_override.as_ref().map(|(id, p)| (*id, p.as_path())),
    ) {
        Ok(r) => r,
        Err(err) => {
            warn!(?err, dir = %global.display(), "skill scan failed; callable selection will be empty");
            return Vec::new();
        }
    };
    let selected = registry.list_for_group(agent_group_id, selector);
    let mut out = Vec::with_capacity(selected.len());
    for skill in &selected {
        if exclude_names.contains(&skill.name.as_str()) {
            continue;
        }
        let body = match copperclaw_skills::read_skill_body(skill) {
            Ok(b) => b,
            Err(err) => {
                warn!(skill = %skill.name, ?err, "skill body read failed; skipping in catalogue");
                continue;
            }
        };
        out.push(SkillCatalogueEntry {
            name: skill.name.clone(),
            description: skill.description.clone(),
            body: body.trim_end().to_string(),
        });
    }
    out
}

/// Test-only thin wrapper over [`select_callable_skills`] that returns
/// `None` when no skills are selected.
#[cfg(test)]
pub(crate) fn build_skills_catalogue(
    skills_dir: Option<&Path>,
    groups_dir: Option<&Path>,
    agent_group_id: AgentGroupId,
    selector: &copperclaw_skills::SkillsSelector,
) -> Option<Vec<SkillCatalogueEntry>> {
    let entries = select_callable_skills(skills_dir, groups_dir, agent_group_id, selector, &[]);
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

/// Assemble the agent's system prompt from the global skills directory
/// (optional per-group override), filtered through the group's
/// `SkillsSelector`. Each skill's `SKILL.md` body is inlined as a
/// labelled `<skill>` block; the wrapper tags help the model treat
/// each one as a discrete unit while keeping the underlying markdown
/// intact.
///
/// Returns an empty string when no skills directory is configured or
/// when the selector resolves to zero skills. Read or parse failures
/// for individual skills are logged and that skill is dropped —
/// failing to load a single skill does not cause the whole spawn to
/// fail.
pub(crate) fn build_skill_system_prompt(
    skills_dir: Option<&std::path::Path>,
    groups_dir: Option<&std::path::Path>,
    agent_group_id: AgentGroupId,
    selector: &copperclaw_skills::SkillsSelector,
    mode: SkillsMode,
    exclude_names: &[&str],
) -> String {
    let Some(global) = skills_dir else {
        return String::new();
    };
    let group_override = groups_dir
        .map(|root| {
            root.join(agent_group_id.as_uuid().to_string())
                .join("skills")
        })
        .filter(|p| p.is_dir())
        .map(|p| (agent_group_id, p));

    let registry = match copperclaw_skills::SkillRegistry::scan(
        global,
        group_override.as_ref().map(|(id, p)| (*id, p.as_path())),
    ) {
        Ok(r) => r,
        Err(err) => {
            warn!(?err, dir = %global.display(), "skill scan failed; system prompt will be empty");
            return String::new();
        }
    };

    let selected = registry.list_for_group(agent_group_id, selector);
    if selected.is_empty() {
        return String::new();
    }

    let mut out = String::with_capacity(8 * 1024);
    match mode {
        SkillsMode::Inline => {
            out.push_str(
                "The following skills document the capabilities available to you. \
Each <skill> block is the rendered SKILL.md for one capability — read \
them all before deciding which tool to call.\n",
            );
            let mut emitted = 0usize;
            for skill in &selected {
                if exclude_names.contains(&skill.name.as_str()) {
                    continue;
                }
                let body = match copperclaw_skills::read_skill_body(skill) {
                    Ok(b) => b,
                    Err(err) => {
                        warn!(
                            skill = %skill.name,
                            ?err,
                            "skill body read failed; skipping"
                        );
                        continue;
                    }
                };
                out.push_str("\n<skill name=\"");
                out.push_str(&escape_attr(&skill.name));
                out.push_str("\" description=\"");
                out.push_str(&escape_attr(&skill.description));
                out.push_str("\">\n");
                out.push_str(body.trim_end());
                out.push_str("\n</skill>\n");
                emitted += 1;
            }
            // If every entry was filtered out, return empty so the caller
            // doesn't emit a header with nothing under it.
            if emitted == 0 {
                return String::new();
            }
        }
        SkillsMode::Callable => {
            // Hand off to the catalogue-backed renderer so the prompt
            // index and the on-disk `skills.json` cannot disagree about
            // which skills exist (any whose body fails to read is dropped
            // from both).
            let catalogue = select_callable_skills(
                skills_dir,
                groups_dir,
                agent_group_id,
                selector,
                exclude_names,
            );
            if catalogue.is_empty() {
                return String::new();
            }
            return render_callable_skill_index(&catalogue);
        }
    }
    out
}

/// Emit the Callable-mode prompt section from a pre-built catalogue.
/// Used both by `build_skill_system_prompt` (which builds the catalogue
/// itself) and `runner_config_for` (which reuses the catalogue it
/// already built for the `skills.json` write).
pub(crate) fn render_callable_skill_index(catalogue: &[SkillCatalogueEntry]) -> String {
    let mut out = String::with_capacity(2 * 1024);
    out.push_str(
        "The following is the catalogue of skills available to you. Each \
<skill> entry shows only the skill's name and one-line description — the full \
SKILL.md body is not inlined. To read a skill's body before acting on it, call \
the `load_skill` tool with that skill's `name`; the tool returns the same \
markdown that would have been inlined.\n",
    );
    for entry in catalogue {
        out.push_str("\n<skill name=\"");
        out.push_str(&escape_attr(&entry.name));
        out.push_str("\" description=\"");
        out.push_str(&escape_attr(&entry.description));
        out.push_str("\" />\n");
    }
    out
}

/// Minimal escape for a description embedded in an XML-like attribute
/// value. We only need to neutralise the quote and ampersand — the
/// agent doesn't parse this strictly, but unbalanced quotes would
/// confuse a casual reader.
pub(crate) fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

/// Best-effort removal of a previous spawn's `skills.json`. `NotFound`
/// is the common case (no prior catalogue) and silently ignored; other
/// errors are logged but never fail the spawn.
pub(crate) fn remove_stale_catalogue(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            warn!(?err, path = %path.display(), "could not remove stale skills catalogue");
        }
    }
}

/// Loosen the per-group memory dir to group-writeable (`0o775`) so the
/// operator can clean up files the container's root user wrote into
/// the bind. Best-effort and no-op on non-unix targets.
pub(crate) fn set_memory_dir_perms(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o775)) {
            warn!(
                ?err,
                path = %path.display(),
                "could not relax per-group memory dir permissions to 0o775"
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Drop a session-local `/data/memory/UNAVAILABLE.md` marker so the
/// agent can detect (from inside the container) that the persistent
/// memory mount was not configured for this spawn. Writes that land in
/// this directory are bound to the session dir, not the per-group dir,
/// so they will not be visible to future sessions of the same group.
pub(crate) fn write_memory_unavailable_marker(
    session_root: &Path,
    intended_src: &Path,
    err: &std::io::Error,
) {
    let dir = session_root.join("memory");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(
            ?e,
            path = %dir.display(),
            "could not create session-local memory dir for UNAVAILABLE marker"
        );
        return;
    }
    let body = format!(
        "# Memory mount unavailable\n\n\
         The per-group memory mount at `{intended}` could not be configured \
         for this session (host error: {err_kind}). Files you write under \
         `/data/memory/` will land in this session's own directory and will \
         **not** persist or be visible to other sessions of this agent group.\n\n\
         If a user references a memory the agent should have, mention that \
         the persistent memory mount is currently unavailable so the operator \
         can investigate.\n",
        intended = intended_src.display(),
        err_kind = err.kind(),
    );
    let marker = dir.join(MEMORY_UNAVAILABLE_FILENAME);
    if let Err(e) = std::fs::write(&marker, body) {
        warn!(
            ?e,
            path = %marker.display(),
            "could not write memory-unavailable marker"
        );
    }
}

/// Translate the db crate's [`container_configs::SkillsSelector`] to
/// the skills crate's [`copperclaw_skills::SkillsSelector`]. They share
/// a JSON shape but are distinct types because the two crates don't
/// (and shouldn't) depend on each other.
pub(crate) fn db_selector_to_skills_selector(
    sel: &container_configs::SkillsSelector,
) -> copperclaw_skills::SkillsSelector {
    match sel {
        container_configs::SkillsSelector::All => copperclaw_skills::SkillsSelector::All,
        container_configs::SkillsSelector::Explicit(names) => {
            copperclaw_skills::SkillsSelector::Explicit(names.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill_md(parent: &std::path::Path, name: &str, body: &str) {
        let dir = parent.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let content = format!("---\nname: {name}\ndescription: desc-of-{name}\n---\n\n{body}");
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn build_skill_system_prompt_empty_when_no_dir() {
        let prompt = build_skill_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            SkillsMode::Inline,
            &[],
        );
        assert!(prompt.is_empty());
    }

    #[test]
    fn build_skill_system_prompt_all_includes_each_skill_body() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "# alpha\nAlpha body\n");
        write_skill_md(&skills, "beta", "# beta\nBeta body\n");
        let prompt = build_skill_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            SkillsMode::Inline,
            &[],
        );
        assert!(prompt.contains("<skill name=\"alpha\""));
        assert!(prompt.contains("Alpha body"));
        assert!(prompt.contains("<skill name=\"beta\""));
        assert!(prompt.contains("Beta body"));
        assert!(prompt.contains("desc-of-alpha"));
        // Frontmatter delimiters must not leak into the prompt.
        assert!(!prompt.contains("---\nname: alpha"));
    }

    #[test]
    fn build_skill_system_prompt_explicit_filters() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        write_skill_md(&skills, "beta", "beta body\n");
        write_skill_md(&skills, "gamma", "gamma body\n");
        let prompt = build_skill_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::Explicit(vec!["beta".into()]),
            SkillsMode::Inline,
            &[],
        );
        assert!(!prompt.contains("alpha body"));
        assert!(prompt.contains("beta body"));
        assert!(!prompt.contains("gamma body"));
    }

    #[test]
    fn build_skill_system_prompt_empty_when_no_skills_selected() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "a\n");
        let prompt = build_skill_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::Explicit(vec![]),
            SkillsMode::Inline,
            &[],
        );
        assert!(prompt.is_empty());
    }

    #[test]
    fn build_skill_system_prompt_group_override_shadows_global() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "send-message", "global body\n");

        let ag = AgentGroupId::new();
        let groups = td.path().join("groups");
        let group_skills = groups.join(ag.as_uuid().to_string()).join("skills");
        std::fs::create_dir_all(&group_skills).unwrap();
        write_skill_md(&group_skills, "send-message", "group override body\n");

        let prompt = build_skill_system_prompt(
            Some(&skills),
            Some(&groups),
            ag,
            &copperclaw_skills::SkillsSelector::All,
            SkillsMode::Inline,
            &[],
        );
        assert!(prompt.contains("group override body"));
        assert!(!prompt.contains("global body"));
    }

    #[test]
    fn build_skill_system_prompt_missing_dir_returns_empty() {
        let prompt = build_skill_system_prompt(
            Some(std::path::Path::new("/definitely/does/not/exist")),
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            SkillsMode::Inline,
            &[],
        );
        assert!(prompt.is_empty());
    }

    #[test]
    fn build_skill_system_prompt_callable_emits_index_without_bodies() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "Alpha body\n");
        write_skill_md(&skills, "beta", "Beta body\n");
        let prompt = build_skill_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            SkillsMode::Callable,
            &[],
        );
        // Names + descriptions present, bodies absent.
        assert!(prompt.contains("name=\"alpha\""));
        assert!(prompt.contains("name=\"beta\""));
        assert!(prompt.contains("desc-of-alpha"));
        assert!(!prompt.contains("Alpha body"));
        assert!(!prompt.contains("Beta body"));
        // The self-closing form makes it visually obvious to the model
        // that the body is *not* here.
        assert!(prompt.contains("\" />"));
        // The instructional sentence reminds the model how to retrieve
        // bodies; the tool name is mentioned literally.
        assert!(prompt.contains("`load_skill`"));
    }

    #[test]
    fn build_skills_catalogue_returns_entries_for_selected_skills() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(
            &skills,
            "alpha",
            "Alpha body line one\nAlpha body line two\n",
        );
        write_skill_md(&skills, "beta", "Beta body\n");
        let entries = build_skills_catalogue(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "alpha"
            && e.description == "desc-of-alpha"
            && e.body.contains("Alpha body line one")));
        assert!(
            entries
                .iter()
                .any(|e| e.name == "beta" && e.body.contains("Beta body"))
        );
    }

    #[test]
    fn build_skills_catalogue_returns_none_with_empty_selector() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "Alpha body\n");
        let entries = build_skills_catalogue(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::Explicit(vec![]),
        );
        assert!(entries.is_none());
    }

    #[test]
    fn build_skills_catalogue_returns_none_without_skills_dir() {
        let entries = build_skills_catalogue(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
        );
        assert!(entries.is_none());
    }

    fn fixed_now() -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(2026, 5, 22, 12, 0, 0).unwrap()
    }

    #[test]
    fn assemble_system_prompt_includes_universal_preamble_and_environment() {
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            None,
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        // Preamble is mode-agnostic — these phrases anchor it.
        assert!(prompt.contains("You are a Copperclaw agent"));
        assert!(prompt.contains("Acting with care"));
        assert!(prompt.contains("Picking tools"));
        assert!(prompt.contains("Never use emojis"));
        // Environment block follows.
        assert!(prompt.contains("# Environment"));
        assert!(prompt.contains("Today is 2026-05-22"));
        assert!(prompt.contains("Working directory: /data"));
    }

    #[test]
    fn assemble_system_prompt_includes_session_and_group_ids() {
        let session = copperclaw_types::SessionId::new();
        let ag = AgentGroupId::new();
        let prompt = assemble_system_prompt(
            None,
            None,
            ag,
            &copperclaw_skills::SkillsSelector::All,
            None,
            session,
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(prompt.contains(&session.as_uuid().to_string()));
        assert!(prompt.contains(&ag.as_uuid().to_string()));
    }

    #[test]
    fn assemble_system_prompt_includes_assistant_name_when_set() {
        let with_name = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            None,
            copperclaw_types::SessionId::new(),
            fixed_now(),
            Some("Atlas"),
            SkillsMode::Inline,
        );
        assert!(with_name.contains("Assistant name: Atlas"));
        let without_name = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            None,
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(!without_name.contains("Assistant name:"));
    }

    #[test]
    fn assemble_system_prompt_omits_briefing_section_when_absent() {
        let td = tempfile::tempdir().unwrap();
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            Some(td.path()),
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(!prompt.contains("# Project briefing"));
        assert!(!prompt.contains("<briefing"));
    }

    #[test]
    fn assemble_system_prompt_includes_session_briefing_when_present() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(
            td.path().join(PROJECT_BRIEFING_FILENAME),
            "House style: terse, no preamble.\n",
        )
        .unwrap();
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            Some(td.path()),
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(prompt.contains("# Project briefing"));
        assert!(prompt.contains("<briefing source=\"session:"));
        assert!(prompt.contains("House style: terse, no preamble."));
    }

    #[test]
    fn assemble_system_prompt_includes_group_briefing_when_present() {
        let td = tempfile::tempdir().unwrap();
        let ag = AgentGroupId::new();
        let group_dir = td.path().join(ag.as_uuid().to_string());
        std::fs::create_dir_all(&group_dir).unwrap();
        std::fs::write(
            group_dir.join(PROJECT_BRIEFING_FILENAME),
            "This deployment runs the support workload.\n",
        )
        .unwrap();
        let prompt = assemble_system_prompt(
            None,
            Some(td.path()),
            ag,
            &copperclaw_skills::SkillsSelector::All,
            None,
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(prompt.contains("<briefing source=\"group:"));
        assert!(prompt.contains("This deployment runs the support workload."));
    }

    #[test]
    fn assemble_system_prompt_group_then_session_briefings_both_included() {
        let td = tempfile::tempdir().unwrap();
        let ag = AgentGroupId::new();
        let group_dir = td.path().join("groups").join(ag.as_uuid().to_string());
        std::fs::create_dir_all(&group_dir).unwrap();
        std::fs::write(group_dir.join(PROJECT_BRIEFING_FILENAME), "GROUP-LEVEL\n").unwrap();
        let session_dir = td.path().join("sess");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join(PROJECT_BRIEFING_FILENAME),
            "SESSION-LEVEL\n",
        )
        .unwrap();
        let prompt = assemble_system_prompt(
            None,
            Some(&td.path().join("groups")),
            ag,
            &copperclaw_skills::SkillsSelector::All,
            Some(&session_dir),
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        let g_pos = prompt.find("GROUP-LEVEL").expect("group briefing present");
        let s_pos = prompt
            .find("SESSION-LEVEL")
            .expect("session briefing present");
        assert!(
            g_pos < s_pos,
            "group briefing must precede session briefing"
        );
    }

    #[test]
    fn assemble_system_prompt_ignores_empty_briefing_file() {
        let td = tempfile::tempdir().unwrap();
        std::fs::write(td.path().join(PROJECT_BRIEFING_FILENAME), "   \n\n").unwrap();
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            Some(td.path()),
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(!prompt.contains("# Project briefing"));
    }

    #[test]
    fn assemble_system_prompt_skills_section_follows_briefing() {
        let td = tempfile::tempdir().unwrap();
        let skills = td.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        write_skill_md(&skills, "alpha", "alpha body\n");
        std::fs::write(td.path().join(PROJECT_BRIEFING_FILENAME), "BRIEF\n").unwrap();
        let prompt = assemble_system_prompt(
            Some(&skills),
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            Some(td.path()),
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        let brief_pos = prompt.find("BRIEF").expect("brief present");
        let skill_pos = prompt.find("<skill name=\"alpha\"").expect("skill present");
        assert!(brief_pos < skill_pos, "briefing must precede skills");
    }

    #[test]
    fn assemble_system_prompt_works_with_no_skills_dir() {
        // A deployment with zero skills still gets a complete, useful
        // system prompt — preamble + environment, no skill block.
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            None,
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(prompt.contains("You are a Copperclaw agent"));
        assert!(!prompt.contains("<skill name="));
    }

    #[test]
    fn read_project_briefing_returns_none_when_neither_source_present() {
        let td = tempfile::tempdir().unwrap();
        let out = read_project_briefing(Some(td.path()), Some(td.path()), AgentGroupId::new());
        assert!(out.is_none());
    }

    #[test]
    fn environment_block_includes_all_required_fields() {
        let session = copperclaw_types::SessionId::new();
        let ag = AgentGroupId::new();
        let block = environment_block(session, ag, fixed_now(), Some("Atlas"));
        assert!(block.starts_with("\n# Environment\n"));
        assert!(block.contains("Today is 2026-05-22"));
        assert!(block.contains(&format!("Session id: {}", session.as_uuid())));
        assert!(block.contains(&format!("Agent group id: {}", ag.as_uuid())));
        assert!(block.contains("Working directory: /data"));
        assert!(block.contains("Assistant name: Atlas"));
    }

    #[test]
    fn environment_block_trims_whitespace_only_assistant_name() {
        let block = environment_block(
            copperclaw_types::SessionId::new(),
            AgentGroupId::new(),
            fixed_now(),
            Some("   "),
        );
        assert!(!block.contains("Assistant name:"));
    }

    #[test]
    fn escape_attr_neutralises_quote_and_amp() {
        assert_eq!(escape_attr("plain"), "plain");
        assert_eq!(escape_attr("a&b"), "a&amp;b");
        assert_eq!(escape_attr("\"hi\""), "&quot;hi&quot;");
    }

    #[test]
    fn db_selector_conversion_roundtrips() {
        use copperclaw_db::tables::container_configs::SkillsSelector as DbSel;
        assert!(matches!(
            db_selector_to_skills_selector(&DbSel::All),
            copperclaw_skills::SkillsSelector::All
        ));
        let names = vec!["a".to_string(), "b".to_string()];
        let mapped = db_selector_to_skills_selector(&DbSel::Explicit(names.clone()));
        match mapped {
            copperclaw_skills::SkillsSelector::Explicit(out) => assert_eq!(out, names),
            copperclaw_skills::SkillsSelector::All => panic!("expected Explicit, got All"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn assemble_system_prompt_surfaces_briefing_read_error_as_diagnostic() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join(PROJECT_BRIEFING_FILENAME);
        std::fs::write(&path, "secret deployment notes\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Tests running as root would defeat chmod 0; bail in that case.
        if std::fs::read_to_string(&path).is_ok() {
            return;
        }
        let prompt = assemble_system_prompt(
            None,
            None,
            AgentGroupId::new(),
            &copperclaw_skills::SkillsSelector::All,
            Some(td.path()),
            copperclaw_types::SessionId::new(),
            fixed_now(),
            None,
            SkillsMode::Inline,
        );
        assert!(
            prompt.contains("Briefing diagnostics"),
            "expected diagnostics section when briefing was unreadable"
        );
        assert!(
            prompt.contains("could not be read"),
            "expected explanation of the read failure"
        );
        // Restore permissions so tempdir cleanup can drop the file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn render_callable_skill_index_only_emits_entries_from_catalogue() {
        let entries = vec![SkillCatalogueEntry {
            name: "kept".into(),
            description: "the only one".into(),
            body: "body".into(),
        }];
        let out = render_callable_skill_index(&entries);
        assert!(out.contains("name=\"kept\""));
        assert!(!out.contains("name=\"dropped\""));
    }

    #[test]
    fn build_skill_system_prompt_escapes_skill_name_in_both_modes() {
        let weird = copperclaw_skills::Skill {
            id: copperclaw_skills::SkillId("weird".into()),
            name: "weird&\"name".into(),
            description: "desc".into(),
            dir: std::path::PathBuf::from("/nonexistent"),
            allowed_tools: None,
            source: copperclaw_skills::SkillSource::Global,
        };
        // Inline-mode rendering goes through `out.push_str` with the
        // escape — exercise the helper directly to pin the contract.
        let entry = SkillCatalogueEntry {
            name: weird.name.clone(),
            description: weird.description.clone(),
            body: "body".into(),
        };
        let callable_out = render_callable_skill_index(std::slice::from_ref(&entry));
        assert!(
            callable_out.contains("name=\"weird&amp;&quot;name\""),
            "callable mode must escape `&` and `\"` in skill name; got: {callable_out}"
        );
        // The inline path renders manually inside `build_skill_system_prompt`
        // — assert via a focused chunk of the format string.
        let inline_chunk = {
            let mut s = String::new();
            s.push_str("\n<skill name=\"");
            s.push_str(&escape_attr(&weird.name));
            s.push_str("\" description=\"");
            s.push_str(&escape_attr(&weird.description));
            s.push_str("\">\n");
            s
        };
        assert!(
            inline_chunk.contains("name=\"weird&amp;&quot;name\""),
            "inline mode must escape `&` and `\"` in skill name"
        );
    }
}
