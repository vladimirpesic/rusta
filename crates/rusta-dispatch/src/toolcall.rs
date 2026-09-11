//! Fenced tool-call parsing — the §6.1 canonical request format.
//!
//! A tool call is a fenced ` ```tool ` block whose body is one JSON object
//! `{"name": ..., "input": {...}}` or a JSON array of such objects. This is
//! the inverse of the core prompt's example (§6.6); the sub-coder loop (and,
//! from M8 on, the main agent loop) parses completions with it. Malformed
//! blocks never abort a turn: they degrade to a corrective note that is fed
//! back as the next observation (§6.11).

use serde_json::Value;

/// One model-requested tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Wire tool name, as spelled by the model.
    pub name: String,
    /// Tool input object (`{}` when the model omitted it).
    pub input: Value,
}

/// Tool calls parsed from one completion plus corrective notes for the
/// malformed blocks that were skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCalls {
    /// Parsed calls, in document order.
    pub calls: Vec<ToolCall>,
    /// One note per malformed block (§6.11: every note carries a remedy).
    pub notes: Vec<String>,
}

/// Parse every ` ```tool ` fence in `text`.
///
/// Forgiveness rules (small models, §6.3 spirit): leading/trailing whitespace
/// around fences and JSON is ignored; a missing closing fence parses the
/// remainder of the text; JSON that is neither object nor array yields a
/// note, never a panic.
pub fn parse_tool_calls(text: &str) -> ToolCalls {
    let mut out = ToolCalls::default();
    let mut block: Option<Vec<String>> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        match &mut block {
            None => {
                if trimmed.starts_with("```tool") {
                    block = Some(Vec::new());
                }
            }
            Some(lines) => {
                if trimmed.starts_with("```") {
                    parse_block(&lines.join("\n"), &mut out);
                    block = None;
                } else {
                    lines.push(line.to_owned());
                }
            }
        }
    }
    // Unterminated fence at EOF: parse what was gathered.
    if let Some(lines) = block {
        parse_block(&lines.join("\n"), &mut out);
    }
    out
}
/// Parse one block body: a single object or an array of objects.
fn parse_block(body: &str, out: &mut ToolCalls) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        out.notes.push(
            "empty tool block: write one ```tool fence containing {\"name\": ..., \"input\": {...}}."
                .to_owned(),
        );
        return;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Array(items)) => {
            if items.is_empty() {
                out.notes.push(
                    "tool block contained an empty array: pass at least one {\"name\": ..., \"input\": {...}} object.".to_owned(),
                );
            }
            for item in items {
                parse_call(item, out);
            }
        }
        Ok(value) => parse_call(value, out),
        Err(err) => out.notes.push(format!(
            "malformed tool block ({err}): the fence body must be JSON like {{\"name\": \"read\", \"input\": {{\"path\": \"src/main.rs\"}}}}."
        )),
    }
}

/// Extract `name`/`input` from one JSON value.
fn parse_call(value: Value, out: &mut ToolCalls) {
    let Some(obj) = value.as_object() else {
        out.notes.push(
            "tool call is not a JSON object: use {\"name\": ..., \"input\": {...}}.".to_owned(),
        );
        return;
    };
    let Some(name) = obj.get("name").and_then(Value::as_str) else {
        out.notes
            .push("tool call is missing the \"name\" string.".to_owned());
        return;
    };
    let input = match obj.get("input") {
        None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
        Some(value @ Value::Object(_)) => value.clone(),
        Some(_) => {
            out.notes
                .push(format!("tool {name:?}: \"input\" must be a JSON object."));
            return;
        }
    };
    out.calls.push(ToolCall {
        name: name.to_owned(),
        input,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_single_object_form() {
        let parsed = parse_tool_calls(
            "prose\n```tool\n{\"name\": \"read\", \"input\": {\"path\": \"a.rs\"}}\n```\n",
        );
        assert_eq!(
            parsed.calls,
            vec![ToolCall {
                name: "read".to_owned(),
                input: json!({"path": "a.rs"}),
            }]
        );
        assert!(parsed.notes.is_empty());
    }

    #[test]
    fn parses_array_form_and_multiple_fences() {
        let parsed = parse_tool_calls(
            "```tool\n[{\"name\": \"glob\", \"input\": {}}, {\"name\": \"grep\", \"input\": {\"pattern\": \"x\"}}]\n```\nmid prose\n```tool\n{\"name\": \"ask\", \"input\": {\"question\": \"q?\"}}\n```",
        );
        let names: Vec<&str> = parsed.calls.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["glob", "grep", "ask"]);
    }

    #[test]
    fn malformed_json_yields_remedy_note() {
        let parsed = parse_tool_calls("```tool\n{name: read}\n```");
        assert!(parsed.calls.is_empty());
        assert_eq!(parsed.notes.len(), 1);
        assert!(parsed.notes[0].contains("malformed tool block"));
    }

    #[test]
    fn missing_name_and_non_object_input_yield_notes() {
        let parsed = parse_tool_calls(
            "```tool\n[{\"input\": {}}, {\"name\": \"read\", \"input\": []}]\n```",
        );
        assert!(parsed.calls.is_empty());
        assert_eq!(parsed.notes.len(), 2);
        assert!(parsed.notes[0].contains("missing the \"name\""));
        assert!(parsed.notes[1].contains("\"input\" must be a JSON object"));
    }

    #[test]
    fn omitted_input_defaults_to_empty_object() {
        let parsed = parse_tool_calls("```tool\n{\"name\": \"map_refresh\"}\n```");
        assert_eq!(
            parsed.calls,
            vec![ToolCall {
                name: "map_refresh".to_owned(),
                input: json!({}),
            }]
        );
    }

    #[test]
    fn unterminated_fence_parses_remainder() {
        let parsed = parse_tool_calls("```tool\n{\"name\": \"read\", \"input\": {}}");
        assert_eq!(parsed.calls.len(), 1);
    }

    #[test]
    fn plain_prose_has_no_calls() {
        let parsed = parse_tool_calls("just an answer\n```\ncode\n```\n");
        assert!(parsed.calls.is_empty() && parsed.notes.is_empty());
    }
}
