//! DeepSeek's live model catalogue: `GET {base}/models`
//! (<https://api-docs.deepseek.com/api/list-models>).
//!
//! Each entry says what a model can do: its context window, the most output
//! one response may carry, whether it takes images, and the effort levels its
//! thinking accepts. A new model or a changed limit therefore needs no
//! Rusta release. Fetched entries are cached on disk for a day, beside
//! Kimi's ([`super::catalog_cache`]); [`DeepseekModelInfo::builtin`] is the
//! fallback when no catalogue is at hand.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::LlmError;

use super::catalog_cache::CatalogEntry;
use super::kimi_catalog::EffortLadder;

/// The window every DeepSeek model has documented: 1M tokens, input and
/// output together.
const DEFAULT_CONTEXT_WINDOW: u64 = 1_048_576;

/// The output ceiling every DeepSeek model has documented: 384K tokens, which
/// is also the largest `max_tokens` a request may carry.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 393_216;

/// What one DeepSeek model can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeepseekModelInfo {
    pub id: String,
    /// The display name (`name`), such as `DeepSeek-V4.1-Flash`.
    pub display_name: Option<String>,
    /// The context window, input and output together, in tokens.
    pub context_window: u64,
    /// The most output one response may carry, and the largest `max_tokens`.
    pub max_output_tokens: u32,
    /// Takes images: `input_modalities` lists `image`.
    pub image_in: bool,
    /// The effort levels thinking accepts, in DeepSeek's recommended order,
    /// and the server's default; `None` for a model that does not think.
    /// `none`, which turns thinking off, is never among them.
    pub efforts: Option<EffortLadder>,
}

impl DeepseekModelInfo {
    /// What Rusta knows about `model` without the live catalogue: the two
    /// models as DeepSeek's `GET /models` lists them, and the legacy
    /// `deepseek-v4-flash*` names, which DeepSeek still resolves to Flash.
    /// Any other id is taken as text-only, as DeepSeek's own harness takes a
    /// model that declares no image input, with the window, output ceiling
    /// and effort ladder every listed model has.
    #[must_use]
    pub fn builtin(model: &str) -> Self {
        let lower = model.to_lowercase();
        let (display_name, image_in) = match lower.as_str() {
            "deepseek-flash" => (Some("DeepSeek-V4.1-Flash"), true),
            "deepseek-v4-pro" => (Some("DeepSeek-V4-Pro"), false),
            _ if lower.starts_with("deepseek-v4-flash") => (None, true),
            _ => (None, false),
        };
        Self {
            id: model.to_string(),
            display_name: display_name.map(str::to_string),
            context_window: DEFAULT_CONTEXT_WINDOW,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            image_in,
            efforts: Some(EffortLadder::standard("high")),
        }
    }

    /// The models listed without the live catalogue: V4.1-Flash first, the
    /// model DeepSeek recommends and Rusta's default, then V4-Pro, the
    /// previous generation, still served.
    #[must_use]
    pub fn builtin_models() -> Vec<Self> {
        vec![
            Self::builtin("deepseek-flash"),
            Self::builtin("deepseek-v4-pro"),
        ]
    }

    /// Parse one catalogue entry; `None` for an entry without an id. A field
    /// the entry leaves out keeps the built-in value for its id, except
    /// `effort`: a model listed without one does not think.
    fn from_entry(entry: &Value) -> Option<Self> {
        let id = entry.get("id")?.as_str().filter(|id| !id.is_empty())?;
        let builtin = Self::builtin(id);
        let image_in = entry
            .get("input_modalities")
            .and_then(Value::as_array)
            .map_or(builtin.image_in, |modalities| {
                modalities.iter().any(|m| m.as_str() == Some("image"))
            });
        Some(Self {
            id: id.to_string(),
            display_name: entry
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
                .or(builtin.display_name),
            context_window: entry
                .get("context_window")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .unwrap_or(builtin.context_window),
            max_output_tokens: entry
                .get("max_output_tokens")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(builtin.max_output_tokens),
            image_in,
            efforts: entry.get("effort").and_then(parse_efforts),
        })
    }
}

impl CatalogEntry for DeepseekModelInfo {
    const VENDOR: &'static str = "deepseek";

    fn id(&self) -> &str {
        &self.id
    }

    fn parse_catalog(body: &str) -> Result<Vec<Self>, LlmError> {
        parse_catalog(body)
    }
}

/// `effort`: the levels thinking accepts, and the server's default.
fn parse_efforts(effort: &Value) -> Option<EffortLadder> {
    let valid: Vec<String> = effort
        .get("supported_levels")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|level| !level.is_empty())
        .map(str::to_string)
        .collect();
    if valid.is_empty() {
        return None;
    }
    Some(EffortLadder {
        valid,
        default: effort
            .get("default_level")
            .and_then(Value::as_str)
            .filter(|level| !level.is_empty())
            .map(str::to_string),
    })
}

/// Parse a `/models` response body. Entries without an id are skipped; the
/// server's order is kept.
pub fn parse_catalog(body: &str) -> Result<Vec<DeepseekModelInfo>, LlmError> {
    let value: Value = serde_json::from_str(body)?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Provider("the model list has no `data` array".to_string()))?;
    Ok(entries
        .iter()
        .filter_map(DeepseekModelInfo::from_entry)
        .collect())
}

/// DeepSeek's documented example response, identical to the live list.
#[cfg(test)]
pub(crate) const DOCUMENTED_CATALOG: &str = r#"{"object":"list","data":[
    {"id":"deepseek-flash","object":"model","owned_by":"deepseek","name":"DeepSeek-V4.1-Flash","context_window":1048576,"max_output_tokens":393216,"input_modalities":["text","image"],"output_modalities":["text"],"effort":{"supported_levels":["low","high","max"],"default_level":"high"},"api_capabilities":{"anthropic_messages":{"system_prompt_update":"in-history","tool_update":"addition-only"}}},
    {"id":"deepseek-v4-pro","object":"model","owned_by":"deepseek","name":"DeepSeek-V4-Pro","context_window":1048576,"max_output_tokens":393216,"input_modalities":["text"],"output_modalities":["text"],"effort":{"supported_levels":["low","high","max"],"default_level":"high"},"api_capabilities":{"anthropic_messages":{"system_prompt_update":"leading-only","tool_update":"leading-only"}}}
]}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documented_catalogue_parses() {
        let models = parse_catalog(DOCUMENTED_CATALOG).unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["deepseek-flash", "deepseek-v4-pro"]
        );
        let flash = &models[0];
        assert_eq!(flash.display_name.as_deref(), Some("DeepSeek-V4.1-Flash"));
        assert_eq!(flash.context_window, 1_048_576);
        assert_eq!(flash.max_output_tokens, 393_216);
        assert!(flash.image_in);
        assert_eq!(flash.efforts, Some(EffortLadder::standard("high")));
        assert!(!models[1].image_in);
        assert!(parse_catalog("{}").is_err());
    }

    /// The built-in table agrees with the documented catalogue for every
    /// model on it, and lists them in Rusta's order.
    #[test]
    fn the_builtin_table_matches_the_documented_catalogue() {
        let documented = parse_catalog(DOCUMENTED_CATALOG).unwrap();
        assert_eq!(DeepseekModelInfo::builtin_models(), documented);
    }

    /// Vision follows `input_modalities`, for a listed model and a new one.
    #[test]
    fn vision_follows_the_input_modalities() {
        let body = r#"{"data":[
            {"id":"deepseek-flash","input_modalities":["text"]},
            {"id":"deepseek-next","input_modalities":["image","text"]},
            {"id":"deepseek-quiet","input_modalities":[]}
        ]}"#;
        let models = parse_catalog(body).unwrap();
        assert!(!models[0].image_in, "the catalogue overrides the table");
        assert!(models[1].image_in);
        assert!(!models[2].image_in);
    }

    /// An id DeepSeek has not listed is text-only; the legacy Flash names
    /// keep Flash's image input.
    #[test]
    fn an_unknown_id_is_text_only() {
        let unknown = DeepseekModelInfo::builtin("deepseek-v5-preview");
        assert!(!unknown.image_in);
        assert_eq!(unknown.context_window, 1_048_576);
        assert_eq!(unknown.max_output_tokens, 393_216);
        assert!(DeepseekModelInfo::builtin("deepseek-v4-flash").image_in);
        assert!(DeepseekModelInfo::builtin("DeepSeek-Flash").image_in);
    }

    /// Fields an entry leaves out keep the built-in values; a model listed
    /// without `effort` does not think, and an entry without an id is
    /// skipped.
    #[test]
    fn missing_fields_keep_the_builtin_values() {
        let body = r#"{"data":[
            {"id":"deepseek-flash"},
            {"id":"deepseek-small","context_window":65536,"max_output_tokens":8192,"effort":{"supported_levels":["high"]}},
            {"object":"model"}
        ]}"#;
        let models = parse_catalog(body).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].context_window, 1_048_576);
        assert_eq!(models[0].max_output_tokens, 393_216);
        assert!(models[0].image_in);
        assert_eq!(models[0].efforts, None);
        assert_eq!(models[1].context_window, 65_536);
        assert_eq!(models[1].max_output_tokens, 8_192);
        assert_eq!(
            models[1].efforts,
            Some(EffortLadder {
                valid: vec!["high".into()],
                default: None
            })
        );
    }
}
