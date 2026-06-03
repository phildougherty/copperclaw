//! Normalization of skill `allowed-tools` names to copperclaw MCP tool
//! names.
//!
//! `SKILL.md` frontmatter follows the Claude convention of `PascalCase`
//! tool names (`Read`, `Bash`, `Edit`, `WebSearch`, …). The copperclaw
//! runner dispatches against `snake_case` MCP tool names (`read_file`,
//! `shell`, `edit_file`, `web_search`, …). For a skill's `allowed-tools`
//! list to actually gate dispatch (Phase 1.1), the names must be
//! translated into the names the model emits as `tool_use` calls.
//!
//! [`normalize`] does that translation: it lower-cases, maps the known
//! Claude aliases onto their copperclaw equivalents, and passes through
//! any name that already looks like a copperclaw tool. The mapping is a
//! superset alias table — a single Claude name (e.g. `Edit`) may expand
//! to several copperclaw tools (`edit_file`, `multi_edit`, `apply_patch`)
//! so a skill that says `allowed-tools: [Edit]` doesn't accidentally lose
//! the multi-edit variant.

/// Translate one frontmatter tool name into the copperclaw MCP tool
/// name(s) it authorizes. Returns one or more names; an unrecognized
/// alias passes through lower-cased so future / custom tools still work.
fn aliases_for(raw: &str) -> Vec<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        // Filesystem read.
        "read" | "read_file" | "readfile" => vec!["read_file"],
        "view" | "view_image" | "viewimage" => vec!["view_image"],
        // Filesystem write / edit — a single Claude alias maps onto every
        // copperclaw mutation variant so the skill scope stays usable.
        "write" | "write_file" | "writefile" => vec!["write_file"],
        "edit" | "edit_file" | "str_replace" | "stredit" => {
            vec!["edit_file", "multi_edit", "apply_patch"]
        }
        "multiedit" | "multi_edit" => vec!["multi_edit"],
        "applypatch" | "apply_patch" => vec!["apply_patch"],
        "copy" | "copy_file" | "copyfile" => vec!["copy_file"],
        // Shell / commands.
        "bash" | "shell" | "sh" | "exec" => vec!["shell"],
        // Search / discovery.
        "glob" => vec!["glob"],
        "grep" | "search" => vec!["grep"],
        "websearch" | "web_search" => vec!["web_search"],
        "webfetch" | "web_fetch" | "fetch" => vec!["web_fetch"],
        // Git introspection.
        "gitblame" | "git_blame" => vec!["git_blame"],
        "gitdiff" | "git_diff" => vec!["git_diff"],
        "gitlog" | "git_log" => vec!["git_log"],
        "gitstatus" | "git_status" => vec!["git_status"],
        // Agent orchestration.
        "task" | "explore" => vec!["explore"],
        "create_agent" | "createagent" => vec!["create_agent"],
        // Messaging primitives.
        "send_message" | "sendmessage" => vec!["send_message"],
        "send_file" | "sendfile" => vec!["send_file"],
        // Anything else: pass the lower-cased snake form through. Leaked
        // via the static fallback below so the return type stays
        // `&'static str`-free for unknowns.
        _ => Vec::new(),
    }
}

/// Normalize a skill's raw `allowed-tools` list into the deduplicated set
/// of copperclaw MCP tool names it authorizes. Unknown names pass through
/// in their lower-cased form (so a skill can name a future / custom tool
/// without this table being updated first).
#[must_use]
pub fn normalize(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in raw {
        let mapped = aliases_for(name);
        if mapped.is_empty() {
            // Unknown alias: keep the lower-cased name verbatim.
            let lc = name.trim().to_ascii_lowercase();
            if !lc.is_empty() && !out.contains(&lc) {
                out.push(lc);
            }
        } else {
            for m in mapped {
                let m = m.to_string();
                if !out.contains(&m) {
                    out.push(m);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_maps_to_read_file() {
        assert_eq!(normalize(&["Read".to_string()]), vec!["read_file"]);
    }

    #[test]
    fn bash_maps_to_shell() {
        assert_eq!(normalize(&["Bash".to_string()]), vec!["shell"]);
    }

    #[test]
    fn edit_expands_to_all_mutation_variants() {
        let got = normalize(&["Edit".to_string()]);
        assert!(got.contains(&"edit_file".to_string()));
        assert!(got.contains(&"multi_edit".to_string()));
        assert!(got.contains(&"apply_patch".to_string()));
    }

    #[test]
    fn read_only_skill_excludes_shell() {
        // The headline Phase 1.1 case: `allowed-tools: [Read]` must NOT
        // include `shell`, so the dispatch gate blocks Bash.
        let got = normalize(&["Read".to_string()]);
        assert!(!got.contains(&"shell".to_string()));
        assert_eq!(got, vec!["read_file"]);
    }

    #[test]
    fn snake_case_passthrough() {
        assert_eq!(normalize(&["read_file".to_string()]), vec!["read_file"]);
        assert_eq!(normalize(&["web_search".to_string()]), vec!["web_search"]);
    }

    #[test]
    fn unknown_tool_passes_through_lowercased() {
        assert_eq!(
            normalize(&["SomeFutureTool".to_string()]),
            vec!["somefuturetool"]
        );
    }

    #[test]
    fn dedups_overlapping_aliases() {
        let got = normalize(&["Read".to_string(), "read_file".to_string()]);
        assert_eq!(got, vec!["read_file"]);
    }

    #[test]
    fn empty_and_blank_are_dropped() {
        assert!(normalize(&[String::new(), "   ".to_string()]).is_empty());
    }
}
