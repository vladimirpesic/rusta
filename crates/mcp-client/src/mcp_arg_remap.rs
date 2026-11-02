//! MCP argument remapping and fuzzy tool-name resolution.
//!
//! Mirrors `glm-acp-agent/src/tools/mcp-arg-remap.ts`.
//!
//! Tool naming convention: MCP tools are exposed to the agent as
//! `mcp__<server_name>__<tool_name>`. The builtin tool `web_search` may map to
//! an MCP server such as `zai_web_search`; `resolve_tool_name` uses keyword
//! fallback (`search`, `reader`, `image`/`vision`) when there is no exact match.

use std::collections::HashSet;

/// Agent-side argument names that may differ from upstream MCP property names.
///
/// Each entry maps a name models commonly emit to the candidate property names
/// it may actually correspond to, in priority order. A candidate is only used
/// when it exists in the target tool's `inputSchema.properties` and the model
/// did not already supply that property itself.
///
/// The Devscriptor entries below cover the parameter names its tools use:
/// path-shaped params (`path` / `project_path` / `file_path`), entity lookups
/// (`entity_name`), LSP positions (`character`), result limits (`top_k` vs
/// `limit`), and server ids for the architecture tools.
const ARG_ALIASES: &[(&str, &[&str])] = &[
    // Z.AI web search expects `search_query`; models often emit `query`.
    ("query", &["search_query"]),
    // Z.AI's vision server takes `image_source`; other vision servers name
    // the image `image_url` or `image`.
    ("image_source", &["image_url", "image"]),
    // Path-shaped parameters differ per Devscriptor tool family:
    // scan/analyze take `path`, lsp_arch_* take `project_path`, LSP tools take `file_path`.
    ("path", &["project_path", "file_path"]),
    ("project_path", &["path"]),
    ("file_path", &["path"]),
    ("file", &["file_path", "path"]),
    ("filepath", &["file_path"]),
    ("file_name", &["file_path"]),
    // `scan_codebase` documents `file_extensions` but its schema uses `extensions`.
    ("file_extensions", &["extensions"]),
    ("extensions", &["file_extensions"]),
    // Entity lookups.
    ("name", &["entity_name"]),
    ("entity", &["entity_name"]),
    // LSP positions use `character`, not `column`.
    ("column", &["character"]),
    ("col", &["character"]),
    // `search_context` uses `top_k`; the other search tools use `limit`.
    ("limit", &["top_k", "max_results"]),
    ("top_k", &["limit"]),
    // Architecture tools.
    ("server", &["server_id"]),
    ("servers", &["server_ids"]),
    ("max_age", &["max_age_days"]),
];

/// Resolve the alias target for `key`, if any candidate is a real property that
/// the caller has not already provided.
fn alias_target(
    key: &str,
    properties: &HashSet<&str>,
    supplied: &serde_json::Map<String, serde_json::Value>,
) -> Option<&'static str> {
    let (_, candidates) = ARG_ALIASES.iter().find(|(from, _)| *from == key)?;
    candidates
        .iter()
        .copied()
        .find(|candidate| properties.contains(candidate) && !supplied.contains_key(*candidate))
}

/// Remap argument keys to match the upstream MCP tool's `inputSchema.properties`.
///
/// - If a key already matches a target property, it is passed through unchanged.
/// - Otherwise the alias table is consulted; the first candidate that exists in
///   the target properties and was not already supplied wins.
/// - If no schema is available (`target_properties` empty), arguments are
///   returned unchanged.
pub fn remap_arguments(
    arguments: &serde_json::Value,
    target_properties: &[String],
) -> serde_json::Value {
    let Some(obj) = arguments.as_object() else {
        return arguments.clone();
    };
    if target_properties.is_empty() {
        return arguments.clone();
    }

    let properties: HashSet<&str> = target_properties.iter().map(String::as_str).collect();
    let mut result = serde_json::Map::with_capacity(obj.len());

    for (key, value) in obj {
        if properties.contains(key.as_str()) {
            result.insert(key.clone(), value.clone());
            continue;
        }

        if let Some(alias) = alias_target(key, &properties, obj) {
            result.insert(alias.to_string(), value.clone());
            continue;
        }

        result.insert(key.clone(), value.clone());
    }

    serde_json::Value::Object(result)
}

/// Resolve a requested tool name against the list of available MCP tool names.
///
/// - Exact match takes priority.
/// - Falls back to keyword-based search (`search`, `reader`, `image`, ...).
/// - Returns `None` if no match is found.
pub fn resolve_tool_name(requested: &str, available: &[String]) -> Option<String> {
    if available.is_empty() {
        return Some(requested.to_string());
    }

    if let Some(exact) = available.iter().find(|n| n == &requested) {
        return Some(exact.clone());
    }

    let keywords = extract_keywords(requested);
    for keyword in keywords {
        if let Some(matched) = available
            .iter()
            .find(|n| n.to_lowercase().contains(keyword))
        {
            return Some(matched.clone());
        }
    }

    None
}

fn extract_keywords(name: &str) -> Vec<&'static str> {
    let lower = name.to_lowercase();
    let mut keywords = Vec::new();
    if lower.contains("search") {
        keywords.push("search");
    }
    if lower.contains("reader") {
        keywords.push("reader");
    }
    if lower.contains("image") || lower.contains("vision") || lower.contains("analysis") {
        keywords.push("image");
        keywords.push("vision");
        keywords.push("analysis");
    }
    keywords
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remap_query_to_search_query() {
        let args = serde_json::json!({"query": "rust"});
        let props = vec!["search_query".to_string()];
        let out = remap_arguments(&args, &props);
        assert_eq!(out["search_query"], serde_json::json!("rust"));
        assert!(out.get("query").is_none());
    }

    #[test]
    fn test_passthrough_for_matching_keys() {
        let args = serde_json::json!({"search_query": "rust", "count": 5});
        let props = vec!["search_query".to_string(), "count".to_string()];
        let out = remap_arguments(&args, &props);
        assert_eq!(out, args);
    }

    #[test]
    fn test_no_schema_passthrough() {
        let args = serde_json::json!({"query": "rust"});
        let out = remap_arguments(&args, &[]);
        assert_eq!(out, args);
    }

    #[test]
    fn test_path_maps_to_project_path_for_arch_tools() {
        // lsp_arch_detect takes `project_path`; models often emit `path`.
        let args = serde_json::json!({"path": "/repo"});
        let out = remap_arguments(&args, &["project_path".to_string()]);
        assert_eq!(out["project_path"], serde_json::json!("/repo"));
        assert!(out.get("path").is_none());
    }

    #[test]
    fn test_path_maps_to_file_path_for_lsp_tools() {
        // get_hover takes `file_path`, `line`, `character`.
        let args = serde_json::json!({"path": "/a.rs", "line": 3, "column": 7});
        let props = ["file_path", "line", "character"]
            .map(String::from)
            .to_vec();
        let out = remap_arguments(&args, &props);
        assert_eq!(out["file_path"], serde_json::json!("/a.rs"));
        assert_eq!(out["character"], serde_json::json!(7));
        assert_eq!(out["line"], serde_json::json!(3));
    }

    #[test]
    fn test_scan_codebase_file_extensions_alias() {
        let args = serde_json::json!({"path": "/repo", "file_extensions": ["rs"]});
        let props = ["path", "extensions"].map(String::from).to_vec();
        let out = remap_arguments(&args, &props);
        assert_eq!(out["path"], serde_json::json!("/repo"));
        assert_eq!(out["extensions"], serde_json::json!(["rs"]));
        assert!(out.get("file_extensions").is_none());
    }

    #[test]
    fn test_limit_maps_to_top_k_for_search_context() {
        let args = serde_json::json!({"query": "auth", "limit": 5});
        let props = ["query", "top_k"].map(String::from).to_vec();
        let out = remap_arguments(&args, &props);
        assert_eq!(out["top_k"], serde_json::json!(5));
        // `query` is a real property here, so it must not be remapped to search_query.
        assert_eq!(out["query"], serde_json::json!("auth"));
    }

    #[test]
    fn test_alias_does_not_clobber_explicitly_supplied_property() {
        // Model supplied both `path` and the correct `project_path`; the alias
        // must not overwrite the real value.
        let args = serde_json::json!({"path": "/wrong", "project_path": "/right"});
        let out = remap_arguments(&args, &["project_path".to_string()]);
        assert_eq!(out["project_path"], serde_json::json!("/right"));
    }

    #[test]
    fn test_unknown_key_passes_through_unchanged() {
        let args = serde_json::json!({"totally_unknown": 1});
        let out = remap_arguments(&args, &["path".to_string()]);
        assert_eq!(out["totally_unknown"], serde_json::json!(1));
    }

    #[test]
    fn test_resolve_exact_match() {
        let available = vec!["web_search".to_string(), "web_reader".to_string()];
        assert_eq!(
            resolve_tool_name("web_search", &available),
            Some("web_search".to_string())
        );
    }

    #[test]
    fn test_resolve_by_keyword() {
        let available = vec!["mcp__zai_web_search__web_search".to_string()];
        assert_eq!(
            resolve_tool_name("search", &available),
            Some("mcp__zai_web_search__web_search".to_string())
        );
    }

    #[test]
    fn test_resolve_returns_none_when_no_match() {
        let available = vec!["foo".to_string()];
        assert!(resolve_tool_name("bar", &available).is_none());
    }
}
