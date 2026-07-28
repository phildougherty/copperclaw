//! Skill discovery and per-group override.
//!
//! Scans a "global" directory and an optional "group override" directory for
//! skills (directories containing a `SKILL.md`). Group skills override global
//! skills with the same name.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use copperclaw_types::AgentGroupId;
use serde::{Deserialize, Serialize};

use crate::error::SkillError;
use crate::frontmatter;
use crate::name;

/// Slug-style identifier for a skill (kebab-case, length \u{2264} 64).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SkillId(pub String);

impl SkillId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SkillId {
    fn from(s: &str) -> Self {
        SkillId(s.to_string())
    }
}

/// Where a skill was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// Loaded from the global skills directory.
    Global,
    /// Loaded from a per-group override directory.
    Group(AgentGroupId),
}

/// A successfully discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub id: SkillId,
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
    pub allowed_tools: Option<Vec<String>>,
    pub source: SkillSource,
}

impl Skill {
    /// The skill's `allowed-tools` frontmatter normalized into the
    /// copperclaw MCP tool names the runner dispatches against (see
    /// [`crate::tool_names::normalize`]).
    ///
    /// Returns `None` when the skill declared no `allowed-tools` (i.e.
    /// the skill imposes no tool scope — the group profile alone bounds
    /// it). Returns `Some(names)` otherwise; the runner feeds this into
    /// its per-turn tool policy so a skill declaring
    /// `allowed-tools: [Read]` blocks `Bash`/`shell` at dispatch.
    #[must_use]
    pub fn allowed_tool_names(&self) -> Option<Vec<String>> {
        self.allowed_tools
            .as_ref()
            .map(|raw| crate::tool_names::normalize(raw))
    }
}

/// Selector for which skills should be exposed to an agent group.
///
/// - `All` serializes as the JSON string `"all"` — every discovered skill
///   inlines. This is the **default** (decision **e**): lowest risk, no
///   task context required.
/// - `Explicit(Vec<String>)` serializes as a JSON array of names — only the
///   named skills, in listed order.
/// - `Relevant { query, limit }` serializes as `{"relevant": {"query": ...,
///   "limit": N}}` — an FTS-scored narrowing (M22 S2). Only the up-to-`limit`
///   skills whose `description` is most relevant to `query` inline, cutting
///   the all-skills prompt bloat noted in `copperclaw-runner`'s
///   `compaction.rs`. `query` is the current task text supplied at
///   selection time; `limit` is the cap on how many skills survive. See
///   [`SkillRegistry::select_relevant`] and [`crate::relevance`].
///
/// All three variants mirror `copperclaw_db::tables::container_configs::
/// SkillsSelector` 1:1 (same JSON shapes). Since M24 U1 the host exposes the
/// full selector per group: `groups.config.update` accepts a `skills` field
/// (`cclaw groups skills <id> ...` / `cclaw groups config update --field
/// 'skills=...'`), persisted in `container_configs.skills` and resolved at
/// spawn by the container manager's prompt assembly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillsSelector {
    All,
    Explicit(Vec<String>),
    /// FTS-scored narrowing: inline only the top-`limit` skills whose
    /// `description` is relevant to `query`.
    Relevant {
        query: String,
        limit: usize,
    },
}

impl Serialize for SkillsSelector {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap as _;
        match self {
            SkillsSelector::All => ser.serialize_str("all"),
            SkillsSelector::Explicit(v) => v.serialize(ser),
            SkillsSelector::Relevant { query, limit } => {
                #[derive(Serialize)]
                struct RelevantBody<'a> {
                    query: &'a str,
                    limit: usize,
                }
                let mut map = ser.serialize_map(Some(1))?;
                map.serialize_entry(
                    "relevant",
                    &RelevantBody {
                        query,
                        limit: *limit,
                    },
                )?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for SkillsSelector {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let v = serde_json::Value::deserialize(de)?;
        match v {
            serde_json::Value::String(s) if s == "all" => Ok(SkillsSelector::All),
            serde_json::Value::String(other) => Err(D::Error::custom(format!(
                "expected \"all\", got \"{other}\""
            ))),
            serde_json::Value::Array(_) => serde_json::from_value::<Vec<String>>(v)
                .map(SkillsSelector::Explicit)
                .map_err(D::Error::custom),
            serde_json::Value::Object(ref map) if map.contains_key("relevant") => {
                #[derive(Deserialize)]
                struct RelevantBody {
                    #[serde(default)]
                    query: String,
                    limit: usize,
                }
                let body: RelevantBody =
                    serde_json::from_value(map["relevant"].clone()).map_err(D::Error::custom)?;
                Ok(SkillsSelector::Relevant {
                    query: body.query,
                    limit: body.limit,
                })
            }
            other => Err(D::Error::custom(format!(
                "expected \"all\", a JSON array, or {{\"relevant\": ...}}, got {other}"
            ))),
        }
    }
}

/// In-memory map of skills, keyed by name. Group skills shadow global ones
/// with the same name.
#[derive(Debug, Default, Clone)]
pub struct SkillRegistry {
    skills: BTreeMap<String, Skill>,
}

impl SkillRegistry {
    /// Scan a global directory and (optionally) a per-group override directory.
    ///
    /// Both arguments are optional in the sense that a `None`-equivalent
    /// (passing a path that does not exist) yields zero contributions; that
    /// case still returns `Ok(_)` with whatever was discovered on the other
    /// side. Missing directories are tolerated; permission errors are not.
    ///
    /// Returns the merged registry. When a skill name appears in both the
    /// global directory and the group override, the group version wins and
    /// is tagged with [`SkillSource::Group`].
    ///
    /// # Errors
    /// - [`SkillError::Io`] if reading a directory listed below the root
    ///   fails for reasons other than not existing.
    /// - Any error returned by [`load_skill`] for a malformed skill.
    pub fn scan(
        global_dir: &Path,
        group_overrides: Option<(AgentGroupId, &Path)>,
    ) -> Result<Self, SkillError> {
        let mut skills: BTreeMap<String, Skill> = BTreeMap::new();

        for entry in scan_dir(global_dir)? {
            let skill = load_skill(&entry, SkillSource::Global)?;
            skills.insert(skill.name.clone(), skill);
        }

        if let Some((group_id, dir)) = group_overrides {
            for entry in scan_dir(dir)? {
                let skill = load_skill(&entry, SkillSource::Group(group_id))?;
                skills.insert(skill.name.clone(), skill);
            }
        }

        Ok(Self { skills })
    }

    /// Number of skills in the registry.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the registry has zero skills.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Iterate all skills in name-sorted order.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// Look up a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// Apply a [`SkillsSelector`] to produce the filtered list for an
    /// agent group. The selector controls which skills are exposed; the
    /// `_ag` parameter is accepted to allow future hooks (e.g. tracing or
    /// per-group telemetry) without breaking the signature.
    ///
    /// `SkillsSelector::All` returns every skill in the registry.
    /// `SkillsSelector::Explicit(names)` returns only the named skills, in
    /// the order they appear in `names`. Names that don't exist in the
    /// registry are **skipped** (and logged at `warn`); this keeps a group
    /// usable when a referenced skill has been removed.
    /// `SkillsSelector::Relevant { query, limit }` returns the FTS-scored
    /// subset (see [`Self::select_relevant`]); an off-topic `query` yields
    /// fewer skills than `All`, shrinking the inlined prompt.
    pub fn list_for_group(&self, _ag: AgentGroupId, selector: &SkillsSelector) -> Vec<Skill> {
        match selector {
            SkillsSelector::All => self.skills.values().cloned().collect(),
            SkillsSelector::Explicit(names) => {
                let mut out = Vec::with_capacity(names.len());
                for n in names {
                    match self.skills.get(n) {
                        Some(s) => out.push(s.clone()),
                        None => {
                            tracing::warn!(skill = %n, "selector references unknown skill; skipping");
                        }
                    }
                }
                out
            }
            SkillsSelector::Relevant { query, limit } => self.select_relevant(query, *limit),
        }
    }

    /// Rank the registry's skills by how relevant each one's `description` is
    /// to `query` and return the top `limit`, most-relevant first.
    ///
    /// Reuses the memory store's full-text path via
    /// [`crate::relevance::rank_descriptions`] (in-memory `SQLite` FTS5 + `bm25`,
    /// normalized to `[0, 1]`). Only skills whose description *matched* the
    /// query are returned, so an off-topic query returns **fewer** skills than
    /// [`SkillsSelector::All`] would — the whole point of the narrowing
    /// (decision **e**): inline stays default, relevance trims what inlines.
    ///
    /// Fail-open: an empty/tokenless `query` or an FTS error falls back to the
    /// full registry so a scoring hiccup never silently strips every skill.
    #[must_use]
    pub fn select_relevant(&self, query: &str, limit: usize) -> Vec<Skill> {
        if query.trim().is_empty() {
            // No task context to score against -> behave like `All`.
            return self.skills.values().cloned().collect();
        }
        let items: Vec<(String, String)> = self
            .skills
            .values()
            .map(|s| (s.name.clone(), s.description.clone()))
            .collect();
        match crate::relevance::rank_descriptions(&items, query) {
            Ok(ranked) => ranked
                .into_iter()
                .take(limit)
                .filter_map(|r| self.skills.get(&r.name).cloned())
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "relevance scoring failed; falling back to all skills");
                self.skills.values().cloned().collect()
            }
        }
    }
}

/// Read a discovered skill's `SKILL.md` and return the markdown body
/// (everything after the YAML frontmatter). The frontmatter itself is
/// dropped — callers that need it should consult [`Skill::description`]
/// and friends, which already capture the parsed fields.
///
/// # Errors
/// - [`SkillError::Io`] if the file is unreadable.
pub fn read_skill_body(skill: &Skill) -> Result<String, SkillError> {
    let path = skill.dir.join("SKILL.md");
    let raw = fs::read_to_string(&path).map_err(|e| SkillError::io(&path, e))?;
    Ok(crate::frontmatter::skip_frontmatter(&raw).to_string())
}

/// List immediate subdirectories of `dir`. Treats a missing `dir` as empty.
fn scan_dir(dir: &Path) -> Result<Vec<PathBuf>, SkillError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let read = fs::read_dir(dir).map_err(|e| SkillError::io(dir, e))?;
    let mut out = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| SkillError::io(dir, e))?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|e| SkillError::io(path.clone(), e))?;
        if metadata.is_dir() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Load a single skill from `dir`, given the source attribution.
///
/// # Errors
/// - [`SkillError::MissingSkillMd`] if there is no `SKILL.md` in `dir`.
/// - [`SkillError::Io`] if the file cannot be read.
/// - [`SkillError::Frontmatter`] if the frontmatter is malformed.
/// - [`SkillError::InvalidName`] if the name is invalid or does not match
///   the directory name.
pub fn load_skill(dir: &Path, source: SkillSource) -> Result<Skill, SkillError> {
    let skill_md = dir.join("SKILL.md");
    if !skill_md.is_file() {
        return Err(SkillError::MissingSkillMd(dir.to_path_buf()));
    }
    let body = fs::read_to_string(&skill_md).map_err(|e| SkillError::io(&skill_md, e))?;
    let fm = frontmatter::parse(&body)?;
    name::validate(&fm.name)?;

    let dir_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| SkillError::InvalidName(format!("directory name is not UTF-8: {dir:?}")))?;
    if dir_name != fm.name {
        return Err(SkillError::InvalidName(format!(
            "frontmatter name {:?} does not match directory name {:?}",
            fm.name, dir_name
        )));
    }

    Ok(Skill {
        id: SkillId(fm.name.clone()),
        name: fm.name,
        description: fm.description,
        dir: dir.to_path_buf(),
        allowed_tools: fm.allowed_tools,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_skill(parent: &Path, name: &str, fm_name: &str, description: &str) {
        let dir = parent.join(name);
        fs::create_dir_all(&dir).unwrap();
        let body = format!("---\nname: {fm_name}\ndescription: {description}\n---\n# body\n");
        fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    fn write_skill_with_tools(parent: &Path, name: &str, tools: &[&str]) {
        let dir = parent.join(name);
        fs::create_dir_all(&dir).unwrap();
        let tools_yaml = format!(
            "[{}]",
            tools
                .iter()
                .map(|t| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let body = format!("---\nname: {name}\ndescription: d\nallowed-tools: {tools_yaml}\n---\n");
        fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    #[test]
    fn skill_id_display_and_from_str() {
        let id = SkillId::from("foo");
        assert_eq!(id.as_str(), "foo");
        assert_eq!(id.to_string(), "foo");
    }

    #[test]
    fn skill_id_ord_and_serde() {
        let a = SkillId::from("a");
        let b = SkillId::from("b");
        assert!(a < b);
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, "\"a\"");
        let back: SkillId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn skill_source_eq() {
        let g = AgentGroupId::new();
        assert_eq!(SkillSource::Global, SkillSource::Global);
        assert_eq!(SkillSource::Group(g), SkillSource::Group(g));
        assert_ne!(SkillSource::Global, SkillSource::Group(g));
    }

    #[test]
    fn selector_serde_all() {
        let s = SkillsSelector::All;
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"all\"");
        let back: SkillsSelector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn selector_serde_explicit() {
        let s = SkillsSelector::Explicit(vec!["a".into(), "b".into()]);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "[\"a\",\"b\"]");
        let back: SkillsSelector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn selector_serde_relevant() {
        let s = SkillsSelector::Relevant {
            query: "deploy the telegram bot".into(),
            limit: 8,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(
            json,
            "{\"relevant\":{\"query\":\"deploy the telegram bot\",\"limit\":8}}"
        );
        let back: SkillsSelector = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn selector_serde_relevant_query_defaults_empty() {
        // A persisted form that carries only a limit round-trips with an
        // empty query (the query is a runtime concern the host injects).
        let back: SkillsSelector = serde_json::from_str("{\"relevant\":{\"limit\":3}}").unwrap();
        assert_eq!(
            back,
            SkillsSelector::Relevant {
                query: String::new(),
                limit: 3
            }
        );
    }

    #[test]
    fn selector_serde_rejects_unknown_string() {
        let err = serde_json::from_str::<SkillsSelector>("\"none\"").unwrap_err();
        assert!(err.to_string().contains("all"));
    }

    #[test]
    fn selector_serde_rejects_non_string_non_array() {
        let err = serde_json::from_str::<SkillsSelector>("42").unwrap_err();
        assert!(err.to_string().contains("expected"));
    }

    #[test]
    fn scan_no_dirs_is_empty() {
        let td = TempDir::new().unwrap();
        let reg = SkillRegistry::scan(&td.path().join("missing"), None).unwrap();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert_eq!(reg.iter().count(), 0);
    }

    #[test]
    fn scan_only_global() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        write_skill(&global, "alpha", "alpha", "A");
        write_skill(&global, "beta", "beta", "B");

        let reg = SkillRegistry::scan(&global, None).unwrap();
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.get("alpha").unwrap().source, SkillSource::Global);
        assert_eq!(reg.get("beta").unwrap().description, "B");
    }

    #[test]
    fn scan_global_plus_group_override() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        let group = td.path().join("group");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&group).unwrap();

        write_skill(&global, "shared", "shared", "global version");
        write_skill(&global, "global-only", "global-only", "g");
        write_skill(&group, "shared", "shared", "group version");
        write_skill(&group, "group-only", "group-only", "gr");

        let gid = AgentGroupId::new();
        let reg = SkillRegistry::scan(&global, Some((gid, &group))).unwrap();
        assert_eq!(reg.len(), 3);

        let shared = reg.get("shared").unwrap();
        assert_eq!(shared.description, "group version");
        assert_eq!(shared.source, SkillSource::Group(gid));

        assert_eq!(reg.get("global-only").unwrap().source, SkillSource::Global);
        assert_eq!(
            reg.get("group-only").unwrap().source,
            SkillSource::Group(gid)
        );
    }

    #[test]
    fn scan_ignores_non_skill_dirs() {
        // A subdirectory without SKILL.md should produce MissingSkillMd —
        // but the contract of `scan_dir` is just "list dirs". `load_skill`
        // surfaces the error. Verify that.
        let td = TempDir::new().unwrap();
        let dir = td.path().join("global");
        fs::create_dir_all(dir.join("not-a-skill")).unwrap();
        let err = SkillRegistry::scan(&dir, None).unwrap_err();
        assert!(matches!(err, SkillError::MissingSkillMd(_)));
    }

    #[test]
    fn load_skill_captures_allowed_tools() {
        let td = TempDir::new().unwrap();
        write_skill_with_tools(td.path(), "tools-skill", &["Read", "Bash"]);
        let s = load_skill(&td.path().join("tools-skill"), SkillSource::Global).unwrap();
        assert_eq!(s.allowed_tools, Some(vec!["Read".into(), "Bash".into()]));
        assert_eq!(s.id.as_str(), "tools-skill");
    }

    #[test]
    fn allowed_tool_names_normalizes_to_mcp_names() {
        let td = TempDir::new().unwrap();
        write_skill_with_tools(td.path(), "rw-skill", &["Read", "Bash"]);
        let s = load_skill(&td.path().join("rw-skill"), SkillSource::Global).unwrap();
        let names = s.allowed_tool_names().unwrap();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"shell".to_string()));
    }

    #[test]
    fn allowed_tool_names_read_only_blocks_shell() {
        // Headline Phase 1.1 case: `allowed-tools: [Read]` must not
        // include `shell`, so the runner's policy blocks Bash.
        let td = TempDir::new().unwrap();
        write_skill_with_tools(td.path(), "ro-skill", &["Read"]);
        let s = load_skill(&td.path().join("ro-skill"), SkillSource::Global).unwrap();
        let names = s.allowed_tool_names().unwrap();
        assert_eq!(names, vec!["read_file".to_string()]);
        assert!(!names.contains(&"shell".to_string()));
    }

    #[test]
    fn allowed_tool_names_none_when_unset() {
        let td = TempDir::new().unwrap();
        write_skill(td.path(), "no-tools", "no-tools", "d");
        let s = load_skill(&td.path().join("no-tools"), SkillSource::Global).unwrap();
        assert!(s.allowed_tool_names().is_none());
    }

    #[test]
    fn load_skill_missing_skill_md() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("empty");
        fs::create_dir_all(&dir).unwrap();
        let err = load_skill(&dir, SkillSource::Global).unwrap_err();
        assert!(matches!(err, SkillError::MissingSkillMd(_)));
    }

    #[test]
    fn load_skill_name_mismatch_is_error() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("on-disk-name");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: different-name\ndescription: d\n---\n",
        )
        .unwrap();
        let err = load_skill(&dir, SkillSource::Global).unwrap_err();
        match err {
            SkillError::InvalidName(msg) => {
                assert!(msg.contains("does not match"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn load_skill_bad_chars_in_name() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("Bad_Name");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: Bad_Name\ndescription: d\n---\n",
        )
        .unwrap();
        let err = load_skill(&dir, SkillSource::Global).unwrap_err();
        assert!(matches!(err, SkillError::InvalidName(_)));
    }

    #[test]
    fn load_skill_name_too_long() {
        let td = TempDir::new().unwrap();
        let n = "a".repeat(65);
        let dir = td.path().join(&n);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {n}\ndescription: d\n---\n"),
        )
        .unwrap();
        let err = load_skill(&dir, SkillSource::Global).unwrap_err();
        assert!(matches!(err, SkillError::InvalidName(_)));
    }

    #[test]
    fn load_skill_propagates_frontmatter_error() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("bad-fm");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "no frontmatter here\n").unwrap();
        let err = load_skill(&dir, SkillSource::Global).unwrap_err();
        assert!(matches!(err, SkillError::Frontmatter(_)));
    }

    #[test]
    fn list_for_group_all() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        write_skill(&global, "a", "a", "A");
        write_skill(&global, "b", "b", "B");
        let reg = SkillRegistry::scan(&global, None).unwrap();

        let listed = reg.list_for_group(AgentGroupId::new(), &SkillsSelector::All);
        let names: Vec<_> = listed.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn list_for_group_explicit_preserves_order_and_skips_unknown() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        write_skill(&global, "a", "a", "A");
        write_skill(&global, "b", "b", "B");
        write_skill(&global, "c", "c", "C");
        let reg = SkillRegistry::scan(&global, None).unwrap();

        let sel = SkillsSelector::Explicit(vec!["c".into(), "missing".into(), "a".into()]);
        let listed = reg.list_for_group(AgentGroupId::new(), &sel);
        let names: Vec<_> = listed.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["c", "a"]);
    }

    #[test]
    fn list_for_group_explicit_empty() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        write_skill(&global, "a", "a", "A");
        let reg = SkillRegistry::scan(&global, None).unwrap();
        let listed = reg.list_for_group(AgentGroupId::new(), &SkillsSelector::Explicit(vec![]));
        assert!(listed.is_empty());
    }

    #[test]
    fn select_relevant_ranks_on_topic_first_and_narrows() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        write_skill(
            &global,
            "git-workflow",
            "git-workflow",
            "commit branch and push in git",
        );
        write_skill(
            &global,
            "dataviz",
            "dataviz",
            "build charts and dashboards to visualize data",
        );
        write_skill(
            &global,
            "research",
            "research",
            "fan out web searches and cite a report",
        );
        let reg = SkillRegistry::scan(&global, None).unwrap();

        // On-topic query: the git skill ranks first, unrelated skills drop out.
        let picked = reg.select_relevant("help me commit my branch in git", 10);
        assert_eq!(picked.first().unwrap().name, "git-workflow");
        assert!(picked.iter().all(|s| s.name != "dataviz"));
        assert!(
            picked.len() < reg.len(),
            "off-topic skills excluded -> fewer than All"
        );
    }

    #[test]
    fn select_relevant_off_topic_is_smaller_than_all() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        write_skill(
            &global,
            "git-workflow",
            "git-workflow",
            "commit and branch in git",
        );
        write_skill(
            &global,
            "dataviz",
            "dataviz",
            "build charts to visualize data",
        );
        write_skill(&global, "research", "research", "fan out web searches");
        let reg = SkillRegistry::scan(&global, None).unwrap();

        let all = reg.list_for_group(AgentGroupId::new(), &SkillsSelector::All);
        let relevant = reg.list_for_group(
            AgentGroupId::new(),
            &SkillsSelector::Relevant {
                query: "quantum gardening ornithology".into(),
                limit: 10,
            },
        );
        assert_eq!(all.len(), 3);
        assert!(
            relevant.len() < all.len(),
            "off-topic Relevant must inline fewer skills than All: {} vs {}",
            relevant.len(),
            all.len()
        );
        assert!(relevant.is_empty(), "nothing overlaps the query");
    }

    #[test]
    fn select_relevant_respects_limit() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        for i in 0..5 {
            write_skill(
                &global,
                &format!("skill-{i}"),
                &format!("skill-{i}"),
                "shared deployment keyword body",
            );
        }
        let reg = SkillRegistry::scan(&global, None).unwrap();
        let picked = reg.select_relevant("deployment", 2);
        assert_eq!(picked.len(), 2, "limit caps the number of relevant skills");
    }

    #[test]
    fn select_relevant_empty_query_falls_back_to_all() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        write_skill(&global, "a", "a", "alpha");
        write_skill(&global, "b", "b", "beta");
        let reg = SkillRegistry::scan(&global, None).unwrap();
        // No usable query -> behave like All rather than strip everything.
        let picked = reg.select_relevant("   ", 10);
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn read_skill_body_strips_frontmatter() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("hello");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: hello\ndescription: d\n---\n\n# Hello\nbody text\n",
        )
        .unwrap();
        let skill = load_skill(&dir, SkillSource::Global).unwrap();
        let body = read_skill_body(&skill).unwrap();
        assert_eq!(body, "# Hello\nbody text\n");
    }

    #[test]
    fn read_skill_body_io_error_when_missing() {
        let td = TempDir::new().unwrap();
        let dir = td.path().join("hello");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: hello\ndescription: d\n---\nb\n",
        )
        .unwrap();
        let skill = load_skill(&dir, SkillSource::Global).unwrap();
        // Remove the file after loading to force an IO error on body read.
        fs::remove_file(dir.join("SKILL.md")).unwrap();
        let err = read_skill_body(&skill).unwrap_err();
        assert!(matches!(err, SkillError::Io { .. }));
    }

    #[test]
    fn registry_iter_is_sorted() {
        let td = TempDir::new().unwrap();
        let global = td.path().join("global");
        fs::create_dir_all(&global).unwrap();
        write_skill(&global, "zeta", "zeta", "z");
        write_skill(&global, "alpha", "alpha", "a");
        write_skill(&global, "mu", "mu", "m");
        let reg = SkillRegistry::scan(&global, None).unwrap();
        let names: Vec<_> = reg.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mu", "zeta"]);
    }
}
