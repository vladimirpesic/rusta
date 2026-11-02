use std::sync::Arc;

use agent_core::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccess, ToolAccesses,
};
use agent_core::tools::builtin::read_media::{image_parts, video_parts};
use async_trait::async_trait;
use base64::Engine;
use lm_bridge::{message::ContentPart, tool::Tool};
use rmcp::model::ToolAnnotations;

use crate::client::{McpBlock, McpToolOutput};

use crate::manager::{ServerHandle, infer_accesses, infer_approval_rule};
use crate::mcp_arg_remap::{remap_arguments, resolve_tool_name};

/// Adapter that exposes a single MCP server tool as an agent `ExecutableTool`.
pub struct McpTool {
    server: Arc<ServerHandle>,
    original_name: String,
    definition: Tool,
    annotations: Option<ToolAnnotations>,
}

impl McpTool {
    /// Create a new adapter for an MCP tool.
    ///
    /// `original_name` is the name the server uses; `definition.name` should already be
    /// qualified (e.g. `mcp__<server>__<tool>`).
    pub fn new(
        server: Arc<ServerHandle>,
        original_name: impl Into<String>,
        definition: Tool,
        annotations: Option<ToolAnnotations>,
    ) -> Self {
        Self {
            server,
            original_name: original_name.into(),
            definition,
            annotations,
        }
    }
}

#[async_trait]
impl ExecutableTool for McpTool {
    fn tool_definition(&self) -> &Tool {
        &self.definition
    }

    /// The names models give an argument mapped to the schema's: `name` to
    /// Devscriptor's `entity_name`, `path` to its `file_path` or
    /// `project_path`, `query` to Z.AI's `search_query` ([`remap_arguments`]).
    fn normalize_arguments(&self, input: serde_json::Value) -> serde_json::Value {
        let properties: Vec<String> = self
            .definition
            .parameters
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default();
        remap_arguments(&input, &properties)
    }

    fn accesses(&self, input: &serde_json::Value) -> ToolAccesses {
        infer_accesses(
            &self.definition,
            self.annotations.as_ref(),
            self.server.name(),
        )
        .with_input(input)
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        infer_approval_rule(&self.definition, self.annotations.as_ref())
    }

    /// What the server declares: `readOnlyHint`. A server that says nothing
    /// is not taken at its word for reading only.
    fn is_read_only(&self) -> bool {
        self.annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint)
            .unwrap_or(false)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        tool_call_id: &str,
    ) -> ExecutableToolResult {
        let available = self.server.tool_names().await;
        let resolved = resolve_tool_name(&self.original_name, &available)
            .unwrap_or_else(|| self.original_name.clone());

        // Enforce the same path sandboxing as built-in file tools when the
        // inferred access pattern touches the filesystem.
        let accesses = self.accesses(&input);
        if accesses.iter().any(|a| {
            matches!(
                a,
                ToolAccess::Read(_) | ToolAccess::Write(_) | ToolAccess::ReadWriteTree(_)
            )
        }) && let Some(path) = extract_path(&input)
            && let Err(_e) = ctx.resolve_allowed_path(&path)
        {
            return ExecutableToolResult::error(format!(
                "path {} is outside allowed directories",
                path.display()
            ));
        }

        match self.server.call_tool(&resolved, Some(input)).await {
            // A server signals a failed tool call with `isError` on an otherwise
            // successful response. Reporting it as a success whose text happens
            // to read like an error leaves the model to infer the failure from
            // prose — so it retries a doomed call, or takes the error message as
            // the answer. Surface it as the failure it is.
            Ok(output) => {
                let is_error = output.is_error;
                ExecutableToolResult {
                    output: render_output(output, ctx, tool_call_id).await,
                    is_error,
                    stop_turn: false,
                }
            }
            Err(e) => ExecutableToolResult::error(e.to_string()),
        }
    }
}

/// Shape an MCP result for the model.
///
/// - Text blocks are joined by newlines; a resource link becomes
///   `[resource: <name> <uri>]`.
/// - An image is saved to the spill directory and sent the way `read_media`
///   sends a file — converted and downsampled for the current model, or
///   described by the vision fallback — with a caption naming the saved copy.
/// - Video is sent when the model takes it; audio, video it does not take,
///   and other binary resources are saved and named instead.
/// - `structuredContent` is added unless a text block already carries the
///   same JSON; on its own it becomes the result's text.
async fn render_output(
    output: McpToolOutput,
    ctx: &ExecutableToolContext,
    tool_call_id: &str,
) -> Vec<ContentPart> {
    let mut parts: Vec<ContentPart> = Vec::new();
    let mut text: Vec<String> = Vec::new();
    let mut texts_seen: Vec<String> = Vec::new();
    let flush = |parts: &mut Vec<ContentPart>, text: &mut Vec<String>| {
        if !text.is_empty() {
            parts.push(ContentPart::text(text.join("\n")));
            text.clear();
        }
    };

    for (index, block) in output.blocks.into_iter().enumerate() {
        match block {
            McpBlock::Text(t) => {
                texts_seen.push(t.clone());
                text.push(t);
            }
            McpBlock::Link { uri, name, mime } => {
                let kind = mime.map_or_else(String::new, |mime| format!(" ({mime})"));
                text.push(format!("[resource: {name} {uri}{kind}]"));
            }
            McpBlock::Media { data, mime, uri } => {
                let saved = save(&data, &mime, tool_call_id, index, ctx).await;
                let (bytes, path) = match saved {
                    Ok(saved) => saved,
                    Err(note) => {
                        text.push(note);
                        continue;
                    }
                };
                let source = uri.map_or_else(String::new, |uri| format!(" from {uri}"));
                if mime.starts_with("image/") {
                    match image_parts(bytes, &mime, &path, false, None, ctx).await {
                        Ok(image) => {
                            flush(&mut parts, &mut text);
                            parts.extend(image);
                        }
                        Err(message) => text.push(format!("[image{source}: {message}]")),
                    }
                } else if mime.starts_with("video/") {
                    match video_parts(&bytes, &mime, &path, ctx) {
                        Ok(video) => {
                            flush(&mut parts, &mut text);
                            parts.extend(video);
                        }
                        Err(refusal) => text.push(format!(
                            "[video{source} saved to {}: {}]",
                            path.display(),
                            refusal.message()
                        )),
                    }
                } else {
                    text.push(format!(
                        "[audio{source} ({mime}, {} bytes) saved to {}; none of the models \
                         Rusta talks to takes audio input]",
                        bytes.len(),
                        path.display()
                    ));
                }
            }
            McpBlock::Blob { data, mime, uri } => {
                let mime = mime.unwrap_or_else(|| "application/octet-stream".to_string());
                match save(&data, &mime, tool_call_id, index, ctx).await {
                    Ok((bytes, path)) => text.push(format!(
                        "[binary resource {uri} ({mime}, {} bytes) saved to {}]",
                        bytes.len(),
                        path.display()
                    )),
                    Err(note) => text.push(format!("[binary resource {uri}: {note}]")),
                }
            }
        }
    }

    if let Some(structured) = output.structured {
        let duplicated = texts_seen.iter().any(|t| {
            serde_json::from_str::<serde_json::Value>(t).is_ok_and(|value| value == structured)
        });
        if !duplicated {
            let pretty = serde_json::to_string_pretty(&structured)
                .unwrap_or_else(|_| structured.to_string());
            if parts.is_empty() && text.is_empty() {
                text.push(pretty);
            } else {
                text.push(format!("Structured content:\n{pretty}"));
            }
        }
    }
    flush(&mut parts, &mut text);
    if parts.is_empty() {
        parts.push(ContentPart::text("[the tool returned no content]"));
    }
    parts
}

/// Decode `data` and save it to the spill directory; a note on failure.
async fn save(
    data: &str,
    mime: &str,
    tool_call_id: &str,
    index: usize,
    ctx: &ExecutableToolContext,
) -> Result<(Vec<u8>, std::path::PathBuf), String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| format!("{mime} content that is not valid base64 ({e}) was dropped"))?;
    let extension = lm_bridge::media::extension_for_mime(mime);
    let path = agent_core::tool_output::save_attachment(
        &bytes,
        tool_call_id,
        index,
        extension,
        &ctx.tool_output,
    )
    .await
    .map_err(|e| format!("{mime} content could not be saved ({e}) and was dropped"))?;
    Ok((bytes, path))
}

/// Extension trait that narrows `ToolAccesses` based on actual call arguments.
pub trait AccessInferenceExt {
    /// If a concrete file path can be extracted from `input`, replace the generic
    /// `.` tree access with that path.
    fn with_input(self, input: &serde_json::Value) -> ToolAccesses;
}

impl AccessInferenceExt for ToolAccesses {
    fn with_input(self, input: &serde_json::Value) -> ToolAccesses {
        if self.iter().any(|a| {
            matches!(
                a,
                agent_core::agent_loop::types::ToolAccess::ReadWriteTree(_)
            )
        }) && let Some(path) = extract_path(input)
        {
            return ToolAccesses::read_write_tree(path);
        }
        self
    }
}

fn extract_path(input: &serde_json::Value) -> Option<std::path::PathBuf> {
    let obj = input.as_object()?;
    for key in ["path", "file", "filepath", "cwd", "directory", "dir"] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            return Some(std::path::PathBuf::from(v));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{McpServerConfig, manager::McpConnectionManager};

    /// `name`, as the LSP tools call a symbol, reaches Devscriptor's
    /// `get_relationships` as the `entity_name` its schema requires.
    #[test]
    fn a_name_is_the_entity_name_get_relationships_requires() {
        let config = McpServerConfig {
            name: "devscriptor".into(),
            command: Some("devscriptor".into()),
            ..Default::default()
        };
        let manager = McpConnectionManager::new_without_events(vec![config], Default::default());
        let definition = Tool::new(
            "mcp__devscriptor__get_relationships",
            "Relationships of an entity",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "entity_name": {"type": "string"},
                    "entity_type": {"type": "string"},
                    "file_path": {"type": "string"}
                },
                "required": ["entity_name"]
            }),
        );
        let tool = McpTool::new(
            manager.get("devscriptor").unwrap(),
            "get_relationships",
            definition,
            None,
        );

        let asked = serde_json::json!({"name": "rate", "file_path": "TaxRules.swift"});
        assert_eq!(
            tool.normalize_arguments(asked),
            serde_json::json!({"entity_name": "rate", "file_path": "TaxRules.swift"})
        );
    }
}
