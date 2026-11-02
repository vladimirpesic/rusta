//! Kimi's live model catalogue: `GET {base}/models`.
//!
//! Each entry says what a model can do — context length, thinking, image and
//! video input, dynamic tools, and the effort levels its `thinking.effort`
//! accepts — so a new model or a changed window needs no Rusta release.
//! Fetched entries are cached on disk for a day ([`super::catalog_cache`]);
//! [`KimiModelInfo::builtin`] is the fallback when no catalogue is at hand.
//!
//! The Kimi Code catalogue describes models in the unified schema:
//! `limit.context`, `reasoning {type, effort {valid, default}}`,
//! `modalities.input` and `tools.dynamic_load`. An entry without `limit` is
//! read from the flat fields of the older schema (`context_length`,
//! `supports_thinking_type`, `think_efforts`, …), so an endpoint that serves
//! only those still works. The pay-per-token platform serves only four flat
//! fields (`context_length`, `supports_image_in`, `supports_video_in`,
//! `supports_reasoning`); what a flat entry leaves out keeps the built-in value
//! for its id.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::LlmError;

use super::catalog_cache::CatalogEntry;

/// Whether a model thinks: always, on request, or never (`reasoning.type`,
/// or the flat schema's `supports_thinking_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingSupport {
    /// Always reasons (`only`).
    Only,
    /// Reasons when asked (`both`).
    Both,
    /// Never reasons (`none`, or the flat schema's `no`).
    No,
}

/// The effort levels a model's thinking accepts, and its default: Kimi's
/// `thinking.effort`, DeepSeek's `reasoning_effort`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffortLadder {
    /// Accepted levels, lowest first as the catalogue lists them.
    pub valid: Vec<String>,
    /// The model's own default, when the catalogue names one.
    pub default: Option<String>,
}

impl EffortLadder {
    /// The ladder Kimi and DeepSeek share: `low`, `high`, `max`.
    #[must_use]
    pub fn standard(default: &str) -> Self {
        Self {
            valid: vec!["low".into(), "high".into(), "max".into()],
            default: Some(default.to_string()),
        }
    }

    /// The accepted level nearest `level` (one of `low`, `high`, `max`):
    /// `level` itself when accepted, else the highest accepted level below
    /// it, else the lowest accepted level.
    #[must_use]
    pub fn nearest(&self, level: &str) -> Option<&str> {
        const ORDER: [&str; 3] = ["low", "high", "max"];
        if self.valid.iter().any(|v| v == level) {
            return self.valid.iter().find(|v| *v == level).map(String::as_str);
        }
        let rank = ORDER.iter().position(|l| *l == level)?;
        ORDER[..rank]
            .iter()
            .rev()
            .find_map(|below| self.valid.iter().find(|v| v == below))
            .or_else(|| self.valid.first())
            .map(String::as_str)
    }
}

/// What one Kimi model can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KimiModelInfo {
    pub id: String,
    pub display_name: Option<String>,
    /// Context window, in tokens.
    pub context_length: u64,
    pub thinking: ThinkingSupport,
    pub image_in: bool,
    pub video_in: bool,
    /// Accepts tool declarations on messages.
    pub dynamic_tools: bool,
    /// The levels `thinking.effort` accepts; `None` when it takes none.
    pub efforts: Option<EffortLadder>,
}

impl KimiModelInfo {
    /// What Rusta knows about `model` without the live catalogue: the
    /// Kimi Code models as its catalogue lists them, and the platform's from
    /// Moonshot's model parameter reference.
    ///
    /// - **Thinking.** Every Kimi Code model always thinks, as do the
    ///   platform's K3 series and `kimi-k2.7-code` (whose thinking "cannot be
    ///   disabled"); `kimi-k2.6` thinks on request.
    /// - **Effort.** `low`/`high`/`max`, defaulting to `max` on
    ///   `kimi-for-coding` and the platform's K3 series and to `high` on the
    ///   Kimi Code `k` ids; the K2.7 models take none.
    /// - **Dynamic tools.** `kimi-for-coding`, the `k` ids and the platform's
    ///   K3 series; on the platform's K2 models a message carrying tools
    ///   fails with `tokenization failed`.
    #[must_use]
    pub fn builtin(model: &str) -> Self {
        use super::kimi::KimiChatProvider as Kimi;
        let lower = model.to_lowercase();
        let platform_k3 = is_platform_k3_series(&lower);
        let efforts = if lower == "kimi-for-coding" || platform_k3 {
            Some(EffortLadder::standard("max"))
        } else if is_k_series(&lower) {
            Some(EffortLadder::standard("high"))
        } else {
            None
        };
        let thinking = if lower.starts_with("kimi-for-coding")
            || is_k_series(&lower)
            || platform_k3
            || lower.starts_with("kimi-k2.7-code")
        {
            ThinkingSupport::Only
        } else {
            ThinkingSupport::Both
        };
        Self {
            id: model.to_string(),
            display_name: kimi_coding_display_name(&lower).map(str::to_string),
            context_length: Kimi::context_window_for(model),
            thinking,
            image_in: Kimi::supports_vision_for(model),
            video_in: Kimi::supports_video_for(model),
            dynamic_tools: lower == "kimi-for-coding" || is_k_series(&lower) || platform_k3,
            efforts,
        }
    }

    /// Parse one catalogue entry; `None` for an entry without an id or a
    /// positive context length. An entry with a `limit` object is read in
    /// the unified schema, any other in the flat one before it.
    fn from_entry(entry: &Value) -> Option<Self> {
        let id = entry.get("id")?.as_str().filter(|id| !id.is_empty())?;
        let builtin = Self::builtin(id);
        let display_name = entry
            .get("display_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .or(builtin.display_name.clone());
        let capabilities = match entry.get("limit") {
            Some(limit) => Capabilities::unified(entry, limit)?,
            None => Capabilities::flat(entry, builtin)?,
        };
        Some(Self {
            id: id.to_string(),
            display_name,
            context_length: capabilities.context_length,
            thinking: capabilities.thinking,
            image_in: capabilities.image_in,
            video_in: capabilities.video_in,
            dynamic_tools: capabilities.dynamic_tools,
            efforts: capabilities.efforts,
        })
    }
}

/// What a catalogue entry says a model can do, in either schema.
struct Capabilities {
    context_length: u64,
    thinking: ThinkingSupport,
    image_in: bool,
    video_in: bool,
    dynamic_tools: bool,
    efforts: Option<EffortLadder>,
}

impl Capabilities {
    /// The unified schema: no `reasoning` object, or
    /// `type: none`, is a model that does not think, and an unrecognized
    /// type thinks no more; the effort ladder is whatever `effort.valid`
    /// lists, its default only when listed.
    fn unified(entry: &Value, limit: &Value) -> Option<Self> {
        let context_length = limit.get("context")?.as_u64().filter(|n| *n > 0)?;
        let reasoning = entry.get("reasoning").filter(|r| r.is_object());
        let thinking = match reasoning
            .and_then(|r| r.get("type"))
            .and_then(Value::as_str)
        {
            Some("only") => ThinkingSupport::Only,
            Some("both") => ThinkingSupport::Both,
            _ => ThinkingSupport::No,
        };
        let efforts = reasoning.and_then(|r| r.get("effort")).and_then(|effort| {
            let valid = string_list(effort.get("valid"));
            let default = effort
                .get("default")
                .and_then(Value::as_str)
                .filter(|level| valid.iter().any(|v| v == level))
                .map(str::to_string);
            (!valid.is_empty()).then_some(EffortLadder { valid, default })
        });
        let inputs = string_list(entry.get("modalities").and_then(|m| m.get("input")));
        Some(Self {
            context_length,
            thinking,
            image_in: inputs.iter().any(|input| input == "image"),
            video_in: inputs.iter().any(|input| input == "video"),
            dynamic_tools: entry
                .get("tools")
                .and_then(|tools| tools.get("dynamic_load"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            efforts,
        })
    }

    /// The older flat schema: the three-state `supports_thinking_type`
    /// wins over the `supports_reasoning` boolean, and `think_efforts` counts
    /// only when its `support` is true. A field the entry leaves out keeps
    /// `builtin`'s value: the platform lists a K3 model as merely reasoning,
    /// with no word on its effort levels or dynamic tools.
    fn flat(entry: &Value, builtin: KimiModelInfo) -> Option<Self> {
        let context_length = entry.get("context_length")?.as_u64().filter(|n| *n > 0)?;
        let flag = |key: &str, otherwise: bool| {
            entry.get(key).and_then(Value::as_bool).unwrap_or(otherwise)
        };
        let thinking = match entry.get("supports_thinking_type").and_then(Value::as_str) {
            Some("only") => ThinkingSupport::Only,
            Some("both") => ThinkingSupport::Both,
            Some("no") => ThinkingSupport::No,
            _ if flag("supports_reasoning", true) => builtin.thinking,
            _ => ThinkingSupport::No,
        };
        let efforts = match entry.get("think_efforts") {
            Some(efforts) => flat_efforts(efforts),
            None if thinking == ThinkingSupport::No => None,
            None => builtin.efforts,
        };
        Some(Self {
            context_length,
            thinking,
            image_in: flag("supports_image_in", builtin.image_in),
            video_in: flag("supports_video_in", builtin.video_in),
            dynamic_tools: flag("supports_dynamic_tools", builtin.dynamic_tools),
            efforts,
        })
    }
}

/// The non-empty strings of a JSON array; none for anything else.
fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

impl CatalogEntry for KimiModelInfo {
    const VENDOR: &'static str = "kimi";

    fn id(&self) -> &str {
        &self.id
    }

    fn parse_catalog(body: &str) -> Result<Vec<Self>, LlmError> {
        parse_catalog(body)
    }
}

/// The flat schema's `think_efforts`, which counts only when its `support`
/// is true.
fn flat_efforts(value: &Value) -> Option<EffortLadder> {
    if value.get("support").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let valid = string_list(value.get("valid_efforts"));
    if valid.is_empty() {
        return None;
    }
    Some(EffortLadder {
        valid,
        default: value
            .get("default_effort")
            .and_then(Value::as_str)
            .filter(|level| !level.is_empty())
            .map(str::to_string),
    })
}

/// Whether `model` is one of the short-form flagship ids — `k3`, and any
/// later `k<major>` — which dropped the `kimi-` prefix.
pub(crate) fn is_k_series(model: &str) -> bool {
    let b = model.as_bytes();
    b.len() >= 2 && b[0].eq_ignore_ascii_case(&b'k') && b[1].is_ascii_digit()
}

/// Whether `model` is one of the platform's K3-series ids — `kimi-k3`,
/// `kimi-k3.1-pro`, `kimi-k3.1-pro-highspeed`, and any later
/// `kimi-k<major>` from 3 on — which share one set of request constraints.
pub(crate) fn is_platform_k3_series(model: &str) -> bool {
    let lower = model.to_lowercase();
    let Some(rest) = lower.strip_prefix("kimi-k") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    rest[..digits].parse::<u32>().is_ok_and(|major| major >= 3)
}

/// The name the Kimi Code catalogue gives `model` (lowercased). Its ids do
/// not all say which model they are: `kimi-for-coding` is K2.8 Preview, and
/// its highspeed variant is K2.7. The platform's ids name their model, and its
/// catalogue carries no names.
fn kimi_coding_display_name(model: &str) -> Option<&'static str> {
    Some(match model {
        "kimi-for-coding" => "K2.8 Preview",
        "kimi-for-coding-highspeed" => "K2.7 Code Highspeed",
        "k3" => "K3",
        "k3-256k" => "K3-256k",
        "k3.1-pro" => "K3.1 Pro",
        "k3.1-pro-highspeed" => "K3.1 Pro Highspeed 2x Usage Cost",
        _ => return None,
    })
}

/// Parse a `/models` response body. Entries without an id or a positive
/// context length are skipped; the server's order is kept (its first model is
/// its default).
pub fn parse_catalog(body: &str) -> Result<Vec<KimiModelInfo>, LlmError> {
    let value: Value = serde_json::from_str(body)?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Provider("the model list has no `data` array".to_string()))?;
    Ok(entries
        .iter()
        .filter_map(KimiModelInfo::from_entry)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live Kimi Code catalogue (identical on both hosts): each entry in
    /// both schemas, flat and unified.
    const LIVE: &str = r#"{"object":"list","data":[
        {"id":"kimi-for-coding","created":1761264000,"created_at":"2025-10-24T00:00:00Z","object":"model","display_name":"K2.8 Preview","type":"model","context_length":1048576,"supports_reasoning":true,"supports_image_in":true,"supports_video_in":true,"supports_dynamic_tools":true,"supports_thinking_type":"only","think_efforts":{"support":true,"valid_efforts":["low","high","max"],"default_effort":"max"},"modalities":{"input":["text","image","video"],"output":["text"]},"reasoning":{"type":"only","effort":{"valid":["low","high","max"],"default":"max"}},"tools":{"dynamic_load":true},"limit":{"context":1048576}},
        {"id":"kimi-for-coding-highspeed","created":1761264000,"created_at":"2025-10-24T00:00:00Z","object":"model","display_name":"K2.7 Code Highspeed","type":"model","context_length":262144,"supports_reasoning":true,"supports_image_in":true,"supports_video_in":true,"supports_dynamic_tools":false,"supports_thinking_type":"only","modalities":{"input":["text","image","video"],"output":["text"]},"reasoning":{"type":"only"},"tools":{"dynamic_load":false},"limit":{"context":262144}},
        {"id":"k3","created":1761264000,"created_at":"2025-10-24T00:00:00Z","object":"model","display_name":"K3","type":"model","context_length":1048576,"supports_reasoning":true,"supports_image_in":true,"supports_video_in":true,"supports_dynamic_tools":true,"supports_thinking_type":"only","think_efforts":{"support":true,"valid_efforts":["low","high","max"],"default_effort":"high"},"modalities":{"input":["text","image","video"],"output":["text"]},"reasoning":{"type":"only","effort":{"valid":["low","high","max"],"default":"high"}},"tools":{"dynamic_load":true},"limit":{"context":1048576}},
        {"id":"k3-256k","created":1761264000,"created_at":"2025-10-24T00:00:00Z","object":"model","display_name":"K3-256k","type":"model","context_length":262144,"supports_reasoning":true,"supports_image_in":true,"supports_video_in":false,"supports_dynamic_tools":true,"supports_thinking_type":"only","think_efforts":{"support":true,"valid_efforts":["low","high","max"],"default_effort":"high"},"modalities":{"input":["text","image"],"output":["text"]},"reasoning":{"type":"only","effort":{"valid":["low","high","max"],"default":"high"}},"tools":{"dynamic_load":true},"limit":{"context":262144}},
        {"id":"k3.1-pro","created":1761264000,"created_at":"2025-10-24T00:00:00Z","object":"model","display_name":"K3.1 Pro","type":"model","context_length":1048576,"supports_reasoning":true,"supports_image_in":true,"supports_video_in":true,"supports_dynamic_tools":true,"supports_thinking_type":"only","think_efforts":{"support":true,"valid_efforts":["low","high","max"],"default_effort":"high"},"modalities":{"input":["text","image","video"],"output":["text"]},"reasoning":{"type":"only","effort":{"valid":["low","high","max"],"default":"high"}},"tools":{"dynamic_load":true},"limit":{"context":1048576}},
        {"id":"k3.1-pro-highspeed","created":1761264000,"created_at":"2025-10-24T00:00:00Z","object":"model","display_name":"K3.1 Pro Highspeed 2x Usage Cost","type":"model","context_length":1048576,"supports_reasoning":true,"supports_image_in":true,"supports_video_in":true,"supports_dynamic_tools":true,"supports_thinking_type":"only","think_efforts":{"support":true,"valid_efforts":["low","high","max"],"default_effort":"high"},"modalities":{"input":["text","image","video"],"output":["text"]},"reasoning":{"type":"only","effort":{"valid":["low","high","max"],"default":"high"}},"tools":{"dynamic_load":true},"limit":{"context":1048576}}
    ],"first_id":"kimi-for-coding","last_id":"k3.1-pro-highspeed","has_more":false}"#;

    #[test]
    fn the_live_catalogue_parses() {
        let models = parse_catalog(LIVE).unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            [
                "kimi-for-coding",
                "kimi-for-coding-highspeed",
                "k3",
                "k3-256k",
                "k3.1-pro",
                "k3.1-pro-highspeed"
            ]
        );
        let coding = &models[0];
        assert_eq!(coding.display_name.as_deref(), Some("K2.8 Preview"));
        assert_eq!(coding.context_length, 1_048_576);
        assert_eq!(coding.thinking, ThinkingSupport::Only);
        assert!(coding.image_in && coding.video_in && coding.dynamic_tools);
        assert_eq!(coding.efforts, Some(EffortLadder::standard("max")));
        assert_eq!(models[1].efforts, None);
        assert!(!models[3].video_in);
        let k31 = &models[4];
        assert_eq!(k31.display_name.as_deref(), Some("K3.1 Pro"));
        assert_eq!(k31.context_length, 1_048_576);
        assert_eq!(k31.thinking, ThinkingSupport::Only);
        assert!(k31.image_in && k31.video_in && k31.dynamic_tools);
        assert_eq!(k31.efforts, Some(EffortLadder::standard("high")));
    }

    /// The unified schema alone reads as both do, so the flat fields may go;
    /// and an endpoint serving only the flat schema reads the same.
    #[test]
    fn either_schema_alone_reads_the_same() {
        let both = parse_catalog(LIVE).unwrap();
        let without = |fields: &[&str]| -> String {
            let mut catalogue: Value = serde_json::from_str(LIVE).unwrap();
            for entry in catalogue["data"].as_array_mut().unwrap() {
                let entry = entry.as_object_mut().unwrap();
                for field in fields {
                    entry.remove(*field);
                }
            }
            catalogue.to_string()
        };
        let unified = without(&[
            "context_length",
            "supports_reasoning",
            "supports_image_in",
            "supports_video_in",
            "supports_dynamic_tools",
            "supports_thinking_type",
            "think_efforts",
        ]);
        let flat = without(&["limit", "reasoning", "modalities", "tools"]);
        assert_eq!(parse_catalog(&unified).unwrap(), both);
        assert_eq!(parse_catalog(&flat).unwrap(), both);
    }

    /// The unified rules: no `reasoning`, `type: none` or an unknown
    /// type is a model that does not think; `both` thinks on request; the
    /// ladder is `effort.valid`, its default only when listed; inputs come
    /// from `modalities.input`; an entry needs a positive `limit.context`.
    #[test]
    fn the_unified_schema_rules_apply() {
        let body = r#"{"data":[
            {"id":"plain","limit":{"context":128000}},
            {"id":"none","limit":{"context":128000},"reasoning":{"type":"none"}},
            {"id":"future","limit":{"context":128000},"reasoning":{"type":"adaptive"}},
            {"id":"both","limit":{"context":128000},"reasoning":{"type":"both","effort":{"valid":["low","",7,"high"],"default":"max"}},"modalities":{"input":["text","image"]},"tools":{"dynamic_load":true}},
            {"id":"no-window","limit":{"context":0},"reasoning":{"type":"only"}},
            {"id":"bad-limit","limit":{},"context_length":4096}
        ]}"#;
        let models = parse_catalog(body).unwrap();
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["plain", "none", "future", "both"],
            "an entry needs a positive limit.context, whatever flat field it has"
        );
        for no_thinking in &models[..3] {
            assert_eq!(
                no_thinking.thinking,
                ThinkingSupport::No,
                "{}",
                no_thinking.id
            );
            assert_eq!(no_thinking.efforts, None, "{}", no_thinking.id);
            assert!(!no_thinking.image_in && !no_thinking.video_in && !no_thinking.dynamic_tools);
        }
        let both = &models[3];
        assert_eq!(both.thinking, ThinkingSupport::Both);
        assert_eq!(
            both.efforts,
            Some(EffortLadder {
                valid: vec!["low".into(), "high".into()],
                default: None,
            }),
            "bad levels are filtered, and a default not listed is dropped"
        );
        assert!(both.image_in && !both.video_in && both.dynamic_tools);
    }

    /// The built-in table agrees with the live catalogue for every model on it.
    #[test]
    fn the_builtin_table_matches_the_live_catalogue() {
        for live in parse_catalog(LIVE).unwrap() {
            let builtin = KimiModelInfo::builtin(&live.id);
            assert_eq!(builtin.display_name, live.display_name, "{}", live.id);
            assert_eq!(builtin.context_length, live.context_length, "{}", live.id);
            assert_eq!(builtin.thinking, live.thinking, "{}", live.id);
            assert_eq!(builtin.image_in, live.image_in, "{}", live.id);
            assert_eq!(builtin.video_in, live.video_in, "{}", live.id);
            assert_eq!(builtin.dynamic_tools, live.dynamic_tools, "{}", live.id);
            assert_eq!(builtin.efforts, live.efforts, "{}", live.id);
        }
    }

    /// The flat schema's rules: `think_efforts` counts only when `support`
    /// is true; bad levels are filtered; the `supports_reasoning` boolean
    /// still reads.
    #[test]
    fn the_flat_schema_follows_its_rules() {
        let body = r#"{"data":[
            {"id":"kimi-plain","context_length":128000,"supports_reasoning":true,"think_efforts":{"support":true,"valid_efforts":["low","",42]}},
            {"id":"kimi-no-effort","context_length":128000,"supports_reasoning":true,"think_efforts":{"support":false,"valid_efforts":["low","high"],"default_effort":"high"}},
            {"id":"broken","context_length":0},
            {"context_length":5}
        ]}"#;
        let models = parse_catalog(body).unwrap();
        assert_eq!(
            models.len(),
            2,
            "entries without an id or a window are skipped"
        );
        assert_eq!(
            models[0].efforts,
            Some(EffortLadder {
                valid: vec!["low".into()],
                default: None
            })
        );
        assert_eq!(models[0].thinking, ThinkingSupport::Both);
        assert_eq!(models[1].efforts, None);
        assert!(parse_catalog("{}").is_err());
    }

    /// The platform's model list as its reference documents it: four flat
    /// fields per model. What they leave out — that the K3 series always
    /// thinks, its effort levels, its dynamic tools — keeps the built-in
    /// value, where the flat rules alone would read every model as thinking
    /// on request with no effort and no dynamic tools.
    #[test]
    fn the_platform_catalogue_keeps_what_it_leaves_out() {
        let body = r#"{"object":"list","data":[
            {"id":"kimi-k3.1-pro","object":"model","created":1792540800,"owned_by":"moonshot","context_length":1048576,"supports_image_in":true,"supports_video_in":true,"supports_reasoning":true},
            {"id":"kimi-k2.7-code","object":"model","created":1781481600,"owned_by":"moonshot","context_length":262144,"supports_image_in":true,"supports_video_in":true,"supports_reasoning":true},
            {"id":"kimi-k2.6","object":"model","created":1776211200,"owned_by":"moonshot","context_length":262144,"supports_image_in":true,"supports_video_in":false,"supports_reasoning":true},
            {"id":"kimi-k3.2","object":"model","context_length":1048576,"supports_reasoning":false},
            {"id":"kimi-k3","object":"model","context_length":1048576},
            {"id":"kimi-for-coding","object":"model","context_length":1048576}
        ]}"#;
        let models = parse_catalog(body).unwrap();
        let k31 = &models[0];
        assert_eq!(k31.thinking, ThinkingSupport::Only);
        assert_eq!(k31.efforts, Some(EffortLadder::standard("max")));
        assert!(k31.dynamic_tools && k31.image_in && k31.video_in);
        assert_eq!(k31.display_name, None, "the platform names no models");
        assert_eq!(models[1].thinking, ThinkingSupport::Only);
        assert_eq!(models[1].efforts, None);
        assert!(!models[1].dynamic_tools);
        assert_eq!(models[2].thinking, ThinkingSupport::Both);
        assert!(!models[2].video_in, "what the entry says wins");
        assert_eq!(
            models[3].thinking,
            ThinkingSupport::No,
            "said not to reason"
        );
        assert_eq!(models[3].efforts, None, "a model that does not think");
        assert_eq!(models[4].thinking, ThinkingSupport::Only, "says nothing");
        assert_eq!(models[4].efforts, Some(EffortLadder::standard("max")));
        assert!(models[4].image_in && models[4].video_in && models[4].dynamic_tools);
        assert_eq!(models[5].display_name.as_deref(), Some("K2.8 Preview"));
    }

    /// The platform's models, per its parameter reference: the K3 series
    /// always thinks, defaults to `max` and takes dynamic tools;
    /// `kimi-k2.7-code` always thinks and takes no effort; `kimi-k2.6`
    /// thinks on request. All take images and video.
    #[test]
    fn the_platform_models_are_built_in() {
        for id in ["kimi-k3.1-pro", "kimi-k3.1-pro-highspeed", "kimi-k3"] {
            let model = KimiModelInfo::builtin(id);
            assert_eq!(model.context_length, 1_048_576, "{id}");
            assert_eq!(model.thinking, ThinkingSupport::Only, "{id}");
            assert_eq!(model.efforts, Some(EffortLadder::standard("max")), "{id}");
            assert!(
                model.dynamic_tools && model.image_in && model.video_in,
                "{id}"
            );
        }
        for id in ["kimi-k2.7-code", "kimi-k2.7-code-highspeed"] {
            let model = KimiModelInfo::builtin(id);
            assert_eq!(model.context_length, 262_144, "{id}");
            assert_eq!(model.thinking, ThinkingSupport::Only, "{id}");
            assert_eq!(model.efforts, None, "{id}");
            assert!(
                !model.dynamic_tools && model.image_in && model.video_in,
                "{id}"
            );
        }
        let k26 = KimiModelInfo::builtin("kimi-k2.6");
        assert_eq!(k26.thinking, ThinkingSupport::Both);
        assert!(!k26.dynamic_tools && k26.efforts.is_none());
        assert!(is_platform_k3_series("Kimi-K4-pro"));
        assert!(!is_platform_k3_series("kimi-k2.7-code"));
        assert!(!is_platform_k3_series("k3.1-pro"));
        assert!(!is_platform_k3_series("kimi-for-coding"));
    }

    #[test]
    fn the_nearest_effort_is_chosen() {
        let ladder = EffortLadder {
            valid: vec!["low".into(), "high".into()],
            default: None,
        };
        assert_eq!(ladder.nearest("high"), Some("high"));
        assert_eq!(ladder.nearest("max"), Some("high"));
        assert_eq!(ladder.nearest("low"), Some("low"));
        let only_max = EffortLadder {
            valid: vec!["max".into()],
            default: None,
        };
        assert_eq!(only_max.nearest("low"), Some("max"));
    }

    #[tokio::test]
    async fn catalogues_are_fetched_and_cached_per_host() {
        use super::super::catalog_cache::{CatalogCache, fetch_models};
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/coding/v1/models"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(LIVE))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let cache = CatalogCache::<KimiModelInfo>::new(dir.path());
        let base = format!("{}/coding/v1", server.uri());

        assert!(cache.load(&base).is_none());
        let models = cache
            .refresh(&base, "sk-test", Default::default())
            .await
            .unwrap();
        assert_eq!(models.len(), 6);
        let cached = cache.load(&base).expect("saved");
        assert!(cached.is_fresh());
        assert_eq!(cached.find("K3").unwrap().context_length, 1_048_576);
        assert!(
            cache
                .path_for(&base)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("kimi-models-127.0.0.1")
        );

        let err = fetch_models::<KimiModelInfo>(&base, "wrong", Default::default())
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::ApiStatus { .. }), "{err}");
    }
}
