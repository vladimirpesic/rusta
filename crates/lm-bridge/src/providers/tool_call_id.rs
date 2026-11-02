use std::collections::HashSet;

/// Sanitize a tool-call ID to the allowed character set.
/// OpenAI/Kimi allow `[a-zA-Z0-9_-]` and up to 64 chars.
pub fn sanitize_tool_call_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    sanitized.chars().take(64).collect()
}

/// Deduplicate tool-call IDs within a single response.
pub fn deduplicate_tool_call_ids(ids: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let base = sanitize_tool_call_id(&id);
        let mut candidate = base.clone();
        let mut suffix = 1u32;
        while !seen.insert(candidate.clone()) {
            candidate = format!("{base}_{suffix}");
            suffix += 1;
        }
        out.push(candidate);
    }
    out
}
