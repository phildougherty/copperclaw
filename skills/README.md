# Skills

A "skill" is a single-directory bundle that teaches the agent **when** to
reach for one of the tools its MCP server exposes. Each skill lives at
`skills/<dirname>/SKILL.md`. The file is splice-loaded into every agent's
system prompt at session spawn, so the union of skill bodies is on the
hot path for every container start.

The conventions below are pinned by integration tests in
`crates/ironclaw-skills/tests/coverage.rs`. If you violate one,
`cargo test -p ironclaw-skills` will fail with a pointer back to this
document.

## Directory layout

```
skills/
  <skill-name>/
    SKILL.md          # required
    <supporting>      # optional: scripts, prompts, sample data
```

`<skill-name>` is the canonical id. It must be kebab-case
(`[a-z0-9][a-z0-9-]{0,63}`); the runtime validator at
`crates/ironclaw-skills/src/name.rs` rejects anything else.

## SKILL.md format

```markdown
---
name: my-skill
description: One-sentence summary the registry uses for relevance scoring.
allowed-tools: [Read, Bash]   # optional, see below
---

# my-skill

Body markdown. Explain *when* the agent should reach for this skill, then
the *what* (schema, examples, edge cases).
```

### Required frontmatter fields

- **`name:`** — must equal the parent directory name. Mismatches are a
  hard error; the test `skill_directory_matches_frontmatter_name`
  catches them.
- **`description:`** — at least 30 characters. The skill registry
  scores skills against the agent's current context using this string;
  blank or one-word descriptions starve the scorer.
  Test: `skill_descriptions_are_substantive`.

### Optional frontmatter fields

- **`allowed-tools:`** — a YAML array of MCP tool names this skill is
  allowed to drive. Parsed by `frontmatter::parse` and exposed via
  `Skill::allowed_tools`. Used by the runner to filter the tool surface
  for sessions that selected this skill explicitly.
- **`tools:`** — *reserved*. No skill currently uses this key. If you
  adopt it, every listed name must resolve to a tool returned by
  `ironclaw_mcp::tools::build_tool_set`. Test:
  `skill_tools_frontmatter_when_present_references_real_tools`.

## Body conventions

### Reference real tools, by name, in backticks

Every tool returned by `ironclaw_mcp::tools::build_tool_set` must be
mentioned in at least one `SKILL.md` (test:
`every_registry_tool_appears_in_some_skill`). Conversely, every
backtick-quoted token that *looks* like a tool reference (multi-word
`snake_case` starting with one of the verb prefixes ironclaw tools use:
`send_`, `edit_`, `add_`, `cancel_`, `pause_`, `resume_`, `update_`,
`list_`, `schedule_`, `ask_`, `create_`, `install_`, `git_`, `web_`,
`read_`, `write_`) must resolve to a real tool. Test:
`skill_tool_mentions_resolve_to_registry`.

Schema field names that happen to look tool-like (e.g. `case_insensitive`,
`max_results`, `thread_id`) are not flagged, because they're tracked in
an explicit allowlist inside the test file. If you're adding a new
schema field that matches the verb-prefix heuristic, add it to
`NON_TOOL_TOKEN_ALLOWLIST`.

### Tell the agent *when* to use the tool, not just what it does

The body should contain at least one phrasing that triggers retrieval:
`when`, `use this`, `reach for`, `if you need`, `prefer`, `before`,
`after`. Skills that read like reference docs train the agent poorly.
Test: `skill_bodies_describe_when_to_use` (lenient — at most one skill
may lack a trigger word, currently `discovering-tools` because it is a
meta-skill).

### Stay under the size cap

Each body must be under **8 KiB**. Every container spawn loads every
skill, so a runaway skill bloats every session uniformly. Test:
`skill_bodies_under_size_cap`. The spec target is 4 KiB; the current
ceiling is 8 KiB pending a content-cull pass on the long-form skills
(`explore`, `web-search`, `add-mcp-server`, etc.).

### No template markers or WIP placeholders

The runtime does not expand Jinja-style templates; `{{ var }}` would
land verbatim in the model's context and confuse it. Same for
`<TODO>` and `[PLACEHOLDER]` markers. Test:
`skill_bodies_contain_no_reserved_markers`.

## Loading order

Skills are iterated in alphabetical order by name (via `BTreeMap`).
This is platform-independent; do not rely on filesystem-iteration
order anywhere downstream. Test: `skill_loading_order_is_alphabetical`.

## Adding a new skill

1. Create `skills/<name>/SKILL.md` with the required frontmatter.
2. Run `cargo test -p ironclaw-skills` — the coverage tests will tell
   you immediately if you've broken any convention.
3. If you reference a new MCP tool, make sure it's in
   `ironclaw_mcp::tools::build_tool_set` first. The reverse-coverage
   test (`every_registry_tool_appears_in_some_skill`) will not flag a
   missing skill until the tool is registered.

## Renaming or deleting an MCP tool

1. Update the registry in `crates/ironclaw-mcp/src/tools/mod.rs`.
2. Update every `SKILL.md` that mentions the old name.
3. Run `cargo test -p ironclaw-skills` — both
   `every_registry_tool_appears_in_some_skill` (if you renamed) and
   `skill_tool_mentions_resolve_to_registry` (if you deleted) will
   point at the orphans.
