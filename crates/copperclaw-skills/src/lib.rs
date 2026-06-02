//! Skill discovery, validation, and container materialization.
//!
//! A "skill" is a directory containing a `SKILL.md` file with YAML
//! frontmatter and arbitrary supporting files (scripts, prompts, data).
//!
//! This crate is responsible for:
//!
//! 1. Discovering skills under a global directory and an optional
//!    per-agent-group override directory.
//! 2. Parsing and validating the frontmatter (`name`, `description`,
//!    optional `allowed-tools`).
//! 3. Selecting which skills are exposed to an agent group based on a
//!    [`SkillsSelector`] (`All` or `Explicit(Vec<String>)`).
//! 4. Materializing the chosen skills into a destination directory by
//!    creating symlinks at `<dest>/<skill_id>` pointing to each skill's
//!    source directory.
//!
//! ## Validation rules
//! - Skill names must match `[a-z0-9][a-z0-9-]{0,63}` (kebab-case,
//!   length \u{2264} 64).
//! - Frontmatter must include both `name` and `description`.
//! - `name` must equal the directory name; mismatch is an **error**.
//! - When a [`SkillsSelector::Explicit`] references a skill that does not
//!   exist, it is **skipped** with a `warn` log entry (not an error). This
//!   keeps a group usable after a skill has been removed.
//! - During materialization, every skill's canonical `dir` must lie under
//!   the configured allowed roots (when any are configured). This is a
//!   defense-in-depth check against malicious group overrides pointing at
//!   arbitrary host paths.

pub mod error;
pub mod frontmatter;
pub mod materialize;
pub mod name;
pub mod registry;

pub use error::SkillError;
pub use frontmatter::{Frontmatter, skip_frontmatter};
pub use materialize::{MaterializeOutcome, MaterializeReport, materialize};
pub use registry::{Skill, SkillId, SkillRegistry, SkillSource, SkillsSelector, read_skill_body};
