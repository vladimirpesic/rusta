use serde_json::{Value, json};

use crate::{
    errors::LlmError,
    media::MediaPolicy,
    message::Message,
    provider::{ChatProvider, GenerateOptions, ModelInfo, StreamedMessage, ThinkingConfig},
    tool::Tool,
};

use super::deepseek_catalog::DeepseekModelInfo;
use super::openai_compat::{HttpTimeouts, OpenAiCompatProvider};

/// How a requested reasoning effort maps onto DeepSeek's
/// `none`/`low`/`high`/`max` ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortChoice {
    /// Turn thinking off.
    Off,
    /// One of `low`, `high`, `max`.
    Level(&'static str),
}

/// DeepSeek provider, wrapping the OpenAI-compatible wire layer.
///
/// DeepSeek's chat API is OpenAI-compatible with three real differences,
/// all handled here: hybrid thinking controlled by top-level `thinking` /
/// `reasoning_effort` fields, `max_tokens` (rather than
/// `max_completion_tokens`) capped at the model's output ceiling, and a 402
/// for a spent balance. Everything else — streaming, tool calls,
/// `reasoning_content` echo — is the generic path.
///
/// What each model can do comes from DeepSeek's catalogue (`GET /models`,
/// see [`super::deepseek_catalog`]) when one is given, else from the built-in
/// table: its window, its output ceiling, its effort levels, and whether it
/// takes images. `deepseek-flash` takes JPEG/PNG/GIF/WebP images (up to 600
/// per request, per DeepSeek's docs — the generic per-request byte budget is
/// the only trim Rusta applies, as for Kimi and GLM), in user messages and
/// in tool results (the `read_media` shape, verified against the live API);
/// `deepseek-v4-pro` takes none, and no model takes video.
pub struct DeepseekChatProvider {
    inner: OpenAiCompatProvider,
    info: DeepseekModelInfo,
    /// The live catalogue, when one was given: the models `known_models`
    /// lists.
    catalog: Option<Vec<DeepseekModelInfo>>,
}

impl DeepseekChatProvider {
    /// The output DeepSeek keeps free of the conversation: 256K (262,144)
    /// tokens. Its Claude Code guide compacts at 786,432 tokens of the 1M
    /// window, and its own harness caps output at 256K by default.
    pub const OUTPUT_RESERVATION: u32 = 262_144;

    pub fn new(
        model: impl Into<String>,
        api_key: Option<String>,
        base_url: impl Into<String>,
        timeouts: HttpTimeouts,
    ) -> Result<Self, LlmError> {
        let model = model.into();
        let info = DeepseekModelInfo::builtin(&model);
        let inner = OpenAiCompatProvider::new("deepseek", model, base_url, api_key, timeouts)?
            // DeepSeek's schema declares no `reasoning_details`; a stored array
            // from a gateway session must not reach it.
            .with_reasoning_details_echo(false);
        Ok(Self {
            inner,
            info,
            catalog: None,
        }
        .declare_media())
    }

    /// Take the configured model's capabilities from the live catalogue
    /// (`GET /models`, see [`super::deepseek_catalog`]) instead of the
    /// built-in table, and list the catalogue's models as the known ones. A
    /// model the catalogue does not list keeps its built-in entry.
    #[must_use]
    pub fn with_catalog(mut self, models: Vec<DeepseekModelInfo>) -> Self {
        if let Some(entry) = models
            .iter()
            .find(|entry| entry.id.eq_ignore_ascii_case(self.inner.model_name()))
        {
            self.info = entry.clone();
        }
        self.catalog = Some(models);
        self.declare_media()
    }

    /// Send `headers` with every request, below the provider's own
    /// ([`crate::custom_headers`]).
    #[must_use]
    pub fn with_custom_headers(mut self, headers: &reqwest::header::HeaderMap) -> Self {
        self.inner = self.inner.with_custom_headers(headers);
        self
    }

    /// Declare the model's media to the wire layer, which would otherwise
    /// guess from the name: DeepSeek's image formats where the model takes
    /// images, nothing where it does not.
    fn declare_media(mut self) -> Self {
        self.inner = self
            .inner
            .with_vision(self.info.image_in)
            .with_media_policy(media_policy(&self.info));
        self
    }

    /// Map a requested reasoning effort onto DeepSeek's vocabulary:
    /// `ultra`/`max` → `max`, `high`/`medium`/`xhigh` → `high`,
    /// `low`/`minimum`/`minimal`/`light` → `low`, `none`/`off` → thinking off.
    /// Anything else is `None`, and the field is left out so the model's own
    /// default applies. A level the model does not list is folded onto the
    /// nearest one it does when the request is built.
    pub fn map_reasoning_effort(effort: &str) -> Option<EffortChoice> {
        match effort.trim().to_lowercase().as_str() {
            "ultra" | "max" => Some(EffortChoice::Level("max")),
            "high" | "medium" | "xhigh" => Some(EffortChoice::Level("high")),
            "low" | "minimum" | "minimal" | "light" => Some(EffortChoice::Level("low")),
            "none" | "off" => Some(EffortChoice::Off),
            _ => None,
        }
    }

    /// The DeepSeek-specific request fields for a model with `info`.
    ///
    /// Both fields are top-level, where DeepSeek reads them; the effort is a
    /// string on the request, not nested inside `thinking`.
    fn build_extra_body(options: &GenerateOptions, info: &DeepseekModelInfo) -> Value {
        let mut extra = json!({});

        // `temperature` is deliberately not injected. DeepSeek accepts it but
        // ignores it while thinking is on (which is every current model's
        // default), so a value Rusta chose would silently not apply — the
        // same reasoning by which the Kimi wrapper never injects one. A
        // caller who passes `--temperature` explicitly keeps their choice;
        // the endpoint accepts and ignores it.

        let Some(ladder) = &info.efforts else {
            // A model that does not think gets neither field; there is
            // nothing to switch, and `reasoning_effort` would name a
            // vocabulary it lacks.
            return extra;
        };

        let effort = options
            .reasoning_effort
            .as_deref()
            .and_then(Self::map_reasoning_effort);

        // Off — asked for directly, or as effort `none`/`off`. DeepSeek
        // spells it `thinking: {type: "disabled"}` with `reasoning_effort:
        // "none"`; the two agree, and an off request never carries a real
        // effort.
        if matches!(options.thinking, Some(ThinkingConfig::Disabled))
            || effort == Some(EffortChoice::Off)
        {
            extra["thinking"] = json!({"type": "disabled"});
            extra["reasoning_effort"] = json!("none");
            return extra;
        }

        // Thinking is on by default server-side; saying so explicitly keeps
        // the request's intent legible on the wire, and an explicit
        // `Enabled`/`EnabledExplicit` lands here too.
        extra["thinking"] = json!({"type": "enabled"});
        if let Some(EffortChoice::Level(level)) = effort
            && let Some(level) = ladder.nearest(level)
        {
            extra["reasoning_effort"] = json!(level);
        }
        extra
    }
}

/// Which media a model with `info` takes: DeepSeek's image formats, up to
/// 4096 px per side, when it takes images; nothing when it does not (a
/// text-only model would silently ignore an image it accepted); and no video
/// on either.
fn media_policy(info: &DeepseekModelInfo) -> MediaPolicy {
    use crate::media::{DEEPSEEK_IMAGE_MIMES, DEEPSEEK_MAX_IMAGE_EDGE_PX, VideoSupport};
    if !info.image_in {
        return MediaPolicy {
            image_mimes: &[],
            video: VideoSupport::Unsupported {
                hint: "the current DeepSeek model does not take image or video input",
            },
            max_image_edge_px: None,
        };
    }
    MediaPolicy {
        image_mimes: DEEPSEEK_IMAGE_MIMES,
        video: VideoSupport::Unsupported {
            hint: "no DeepSeek model takes video input",
        },
        max_image_edge_px: Some(DEEPSEEK_MAX_IMAGE_EDGE_PX),
    }
}

#[async_trait::async_trait]
impl ChatProvider for DeepseekChatProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn context_window(&self) -> u64 {
        self.info.context_window
    }

    fn supports_vision(&self) -> bool {
        self.info.image_in
    }

    fn media_policy(&self) -> MediaPolicy {
        media_policy(&self.info)
    }

    /// The live catalogue's models, or the built-in list: V4.1-Flash first.
    fn known_models(&self) -> Vec<ModelInfo> {
        self.catalog
            .clone()
            .unwrap_or_else(DeepseekModelInfo::builtin_models)
            .into_iter()
            .map(|model| ModelInfo {
                id: model.id,
                display_name: model.display_name,
                context_window: model.context_window,
                supports_vision: model.image_in,
            })
            .collect()
    }

    /// The context budget compacts ahead of [`Self::OUTPUT_RESERVATION`], or
    /// of the model's whole output ceiling where that is lower.
    fn output_reservation(&self) -> Option<u64> {
        Some(u64::from(
            Self::OUTPUT_RESERVATION.min(self.info.max_output_tokens),
        ))
    }

    /// With no cap configured, DeepSeek still gets one: the endpoint's own
    /// default is low (8K non-thinking, 64K thinking, 128K at `max` effort),
    /// far below what a 1M-window coding session expects. The cap is
    /// `window − input`, held to the output the window keeps free
    /// ([`Self::output_reservation`]) — the same deliberate divergence from
    /// the generic uncapped default that GLM makes for its 128K limit. An
    /// explicit cap may still reach the model's output ceiling. An unknown
    /// window sends nothing.
    fn completion_tokens_when_unset(&self, input_tokens: u64, context_window: u64) -> Option<u32> {
        if context_window == 0 {
            return None;
        }
        let reservation = self.output_reservation().unwrap_or(u64::MAX);
        let room = context_window
            .saturating_sub(input_tokens)
            .min(reservation)
            .max(1);
        Some(u32::try_from(room).unwrap_or(u32::MAX))
    }

    async fn generate(
        &self,
        system_prompt: Option<&str>,
        tools: &[Tool],
        history: &[Message],
        options: GenerateOptions,
    ) -> Result<StreamedMessage, LlmError> {
        let mut options = options;
        let mut extra_body = Self::build_extra_body(&options, &self.info);

        // DeepSeek documents `max_tokens`, up to the model's output ceiling;
        // `max_completion_tokens` is undocumented, and the generic layer
        // would send it under that name.
        if let Some(max) = options.max_completion_tokens.or(options.max_tokens) {
            let clamped = max.clamp(1, self.info.max_output_tokens);
            extra_body["max_tokens"] = json!(clamped);
            options.max_completion_tokens = None;
            options.max_tokens = None;
        }

        self.inner
            .generate(system_prompt, tools, history, options, extra_body)
            .await
            .map_err(explain_insufficient_balance)
    }
}

/// Reword DeepSeek's HTTP 402 as the business error it is.
///
/// A 402 means the account's balance is spent; no retry fixes that, and the
/// generic classifier would report it as a plain `ApiStatus` 400-series
/// failure. Surfacing it as a business error makes it fatal, names the cause,
/// and points at the fix. Retryable 429s and 5xx are left exactly as the
/// generic path classifies them.
fn explain_insufficient_balance(error: LlmError) -> LlmError {
    match error {
        LlmError::ApiStatus {
            status, message, ..
        } if status == reqwest::StatusCode::PAYMENT_REQUIRED => LlmError::ApiBusinessError {
            code: "402".to_string(),
            message: format!(
                "insufficient DeepSeek balance; top up the account at \
                 https://platform.deepseek.com. DeepSeek: {message}"
            ),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extra(options: GenerateOptions, model: &str) -> Value {
        DeepseekChatProvider::build_extra_body(&options, &DeepseekModelInfo::builtin(model))
    }

    fn with_effort(effort: &str) -> GenerateOptions {
        GenerateOptions {
            reasoning_effort: Some(effort.to_string()),
            ..Default::default()
        }
    }

    /// DeepSeek's effort vocabulary, plus the aliases Rusta folds onto it.
    #[test]
    fn every_documented_effort_alias_maps() {
        for (input, expected) in [
            ("ultra", EffortChoice::Level("max")),
            ("max", EffortChoice::Level("max")),
            ("xhigh", EffortChoice::Level("high")),
            ("high", EffortChoice::Level("high")),
            ("medium", EffortChoice::Level("high")),
            ("low", EffortChoice::Level("low")),
            ("minimum", EffortChoice::Level("low")),
            ("minimal", EffortChoice::Level("low")),
            ("light", EffortChoice::Level("low")),
            (" None ", EffortChoice::Off),
            ("OFF", EffortChoice::Off),
        ] {
            assert_eq!(
                DeepseekChatProvider::map_reasoning_effort(input),
                Some(expected),
                "{input}"
            );
        }
        assert_eq!(DeepseekChatProvider::map_reasoning_effort("turbo"), None);
    }

    /// Thinking is a hybrid feature that defaults ON: a bare request still
    /// says so on the wire.
    #[test]
    fn thinking_is_enabled_by_default() {
        let body = extra(GenerateOptions::default(), "deepseek-v4-pro");
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert!(body.get("reasoning_effort").is_none(), "{body}");
        assert!(body.get("temperature").is_none(), "{body}");
    }

    /// An explicit ask lands at the same place, with the effort top-level.
    #[test]
    fn thinking_explicit_and_effort_are_top_level() {
        let body = extra(
            GenerateOptions {
                thinking: Some(ThinkingConfig::EnabledExplicit),
                reasoning_effort: Some("medium".to_string()),
                ..Default::default()
            },
            "deepseek-v4-pro",
        );
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["reasoning_effort"], json!("high"));
    }

    /// Off is `disabled` + effort `none`, whether asked for directly or as
    /// effort `none`, and never carries a real effort.
    #[test]
    fn thinking_off_is_disabled_with_none_effort() {
        for options in [
            GenerateOptions {
                thinking: Some(ThinkingConfig::Disabled),
                reasoning_effort: Some("high".to_string()),
                ..Default::default()
            },
            GenerateOptions {
                reasoning_effort: Some("none".to_string()),
                ..Default::default()
            },
            GenerateOptions {
                reasoning_effort: Some("off".to_string()),
                ..Default::default()
            },
        ] {
            let body = extra(options, "deepseek-v4-pro");
            assert_eq!(body["thinking"], json!({"type": "disabled"}));
            assert_eq!(body["reasoning_effort"], json!("none"));
        }
    }

    /// An unrecognized effort is omitted, so the model's own default applies.
    #[test]
    fn an_unrecognized_effort_is_left_out() {
        let body = extra(
            GenerateOptions {
                reasoning_effort: Some("turbo".to_string()),
                ..Default::default()
            },
            "deepseek-flash",
        );
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    /// A model the catalogue lists without effort levels does not think: it
    /// gets no thinking fields at all, so it is not sent properties its
    /// schema lacks.
    #[test]
    fn a_non_thinking_model_gets_no_thinking_fields() {
        let info = DeepseekModelInfo {
            efforts: None,
            ..DeepseekModelInfo::builtin("deepseek-future-plain")
        };
        let body = DeepseekChatProvider::build_extra_body(
            &GenerateOptions {
                thinking: Some(ThinkingConfig::EnabledExplicit),
                reasoning_effort: Some("high".to_string()),
                ..Default::default()
            },
            &info,
        );
        assert!(body.get("thinking").is_none(), "{body}");
        assert!(body.get("reasoning_effort").is_none(), "{body}");
    }

    /// Temperature is never injected into the DeepSeek request.
    #[test]
    fn temperature_is_never_injected() {
        for model in ["deepseek-v4-pro", "deepseek-flash"] {
            let body = extra(
                GenerateOptions {
                    thinking: Some(ThinkingConfig::Enabled),
                    ..Default::default()
                },
                model,
            );
            assert!(
                body.get("temperature").is_none(),
                "{model} must not be sent an injected temperature"
            );
        }
    }

    /// With no cap configured, the unset-completion default is the room left
    /// in the window, held to the 256K the window keeps free for output; an
    /// unknown window sends nothing. The budget compacts ahead of those 256K.
    #[test]
    fn unset_completion_tokens_keep_to_the_output_reservation() {
        let provider = DeepseekChatProvider::new(
            "deepseek-flash",
            None,
            "https://api.deepseek.com",
            HttpTimeouts::idle(std::time::Duration::from_secs(5)),
        )
        .unwrap();
        assert_eq!(provider.output_reservation(), Some(262_144));
        // Room left in the window, below the reservation.
        assert_eq!(
            provider.completion_tokens_when_unset(900_000, 1_048_576),
            Some(148_576)
        );
        // Room left above the reservation -> the reservation, at the
        // compaction threshold and below it.
        assert_eq!(
            provider.completion_tokens_when_unset(786_432, 1_048_576),
            Some(262_144)
        );
        assert_eq!(
            provider.completion_tokens_when_unset(0, 1_048_576),
            Some(262_144)
        );
        // A full window still leaves at least one token.
        assert_eq!(
            provider.completion_tokens_when_unset(1_048_576, 1_048_576),
            Some(1)
        );
        assert_eq!(provider.completion_tokens_when_unset(0, 0), None);
    }

    fn local(model: &str) -> DeepseekChatProvider {
        DeepseekChatProvider::new(
            model,
            None,
            "https://api.deepseek.com",
            HttpTimeouts::idle(std::time::Duration::from_secs(5)),
        )
        .unwrap()
    }

    /// Without a catalogue, the built-in list is advertised, Flash first;
    /// with one, the catalogue's models are, and the configured model takes
    /// its window, vision and output ceiling from its catalogue entry.
    #[test]
    fn known_models_follow_the_catalogue_or_the_builtin_table() {
        let ids = |provider: &DeepseekChatProvider| -> Vec<String> {
            provider.known_models().into_iter().map(|m| m.id).collect()
        };
        assert_eq!(
            ids(&local("deepseek-flash")),
            ["deepseek-flash", "deepseek-v4-pro"]
        );

        let mut catalog = super::super::deepseek_catalog::parse_catalog(
            super::super::deepseek_catalog::DOCUMENTED_CATALOG,
        )
        .unwrap();
        catalog[1].image_in = true;
        catalog[1].context_window = 2_097_152;
        catalog[1].max_output_tokens = 65_536;
        let pro = local("deepseek-v4-pro").with_catalog(catalog);
        assert_eq!(ids(&pro), ["deepseek-flash", "deepseek-v4-pro"]);
        assert!(pro.supports_vision(), "vision follows the catalogue");
        assert!(pro.media_policy().accepts_image("image/png"));
        assert_eq!(pro.context_window(), 2_097_152);
        assert_eq!(pro.output_reservation(), Some(65_536));
        assert_eq!(pro.completion_tokens_when_unset(0, 2_097_152), Some(65_536));
    }

    /// An id DeepSeek has not listed is text-only, and its images degrade
    /// to a note; the legacy Flash names keep Flash's image input.
    #[test]
    fn an_unknown_model_is_text_only() {
        let unknown = local("deepseek-v5-preview");
        assert!(!unknown.supports_vision());
        assert!(!unknown.media_policy().takes_images());
        assert_eq!(unknown.context_window(), 1_048_576);
        assert!(local("deepseek-v4-flash").supports_vision());
    }

    /// Media follows the model: Flash takes DeepSeek's image formats (and no
    /// video), v4-pro takes nothing at all.
    #[test]
    fn media_policy_follows_the_model() {
        let flash = local("deepseek-flash").media_policy();
        assert!(flash.accepts_image("image/png"));
        assert!(flash.accepts_image("image/gif"));
        assert!(flash.accepts_image("image/webp"));
        assert!(!flash.accepts_image("image/bmp"));
        assert!(!flash.takes_video());

        let pro = local("deepseek-v4-pro").media_policy();
        assert!(!pro.takes_images());
        assert!(!pro.takes_video());
    }

    /// A requested effort folds onto the levels the model lists: one that
    /// offers only `high` gets `high` for `max` and `low`, and `none` still
    /// turns thinking off.
    #[test]
    fn the_effort_folds_onto_the_models_levels() {
        let only_high = DeepseekModelInfo {
            efforts: Some(super::super::kimi_catalog::EffortLadder {
                valid: vec!["high".into()],
                default: Some("high".into()),
            }),
            ..DeepseekModelInfo::builtin("deepseek-flash")
        };
        for asked in ["max", "low", "high"] {
            let body = DeepseekChatProvider::build_extra_body(&with_effort(asked), &only_high);
            assert_eq!(body["reasoning_effort"], json!("high"), "{asked}");
        }
        let off = DeepseekChatProvider::build_extra_body(&with_effort("none"), &only_high);
        assert_eq!(off["thinking"], json!({"type": "disabled"}));
        assert_eq!(off["reasoning_effort"], json!("none"));
        // The full ladder keeps every level.
        for asked in ["max", "low", "high"] {
            let body = extra(with_effort(asked), "deepseek-flash");
            assert_eq!(body["reasoning_effort"], json!(asked));
        }
    }

    fn chat_response() -> Value {
        json!({
            "id": "chatcmpl-test",
            "choices": [{
                "message": { "role": "assistant", "content": "red" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
    }

    async fn provider_at(server: &wiremock::MockServer) -> DeepseekChatProvider {
        DeepseekChatProvider::new(
            "deepseek-v4-pro",
            Some("sk-test".to_string()),
            server.uri(),
            HttpTimeouts::idle(std::time::Duration::from_secs(5)),
        )
        .unwrap()
    }

    /// DeepSeek reads `max_tokens`, never the OpenAI `max_completion_tokens`,
    /// and an over-limit ask is clamped to the model's output ceiling: the
    /// documented 384K, or what the catalogue says.
    #[tokio::test]
    async fn max_tokens_is_sent_and_clamped() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response()))
            .mount(&server)
            .await;

        let small_ceiling = {
            let mut catalog = super::super::deepseek_catalog::parse_catalog(
                super::super::deepseek_catalog::DOCUMENTED_CATALOG,
            )
            .unwrap();
            catalog[1].max_output_tokens = 65_536;
            catalog
        };
        for (asked, catalog, expected) in [
            (123, None, 123),
            (1_000_000, None, 393_216),
            (1_000_000, Some(small_ceiling), 65_536),
        ] {
            let mut provider = provider_at(&server).await;
            if let Some(catalog) = catalog {
                provider = provider.with_catalog(catalog);
            }
            let _ = provider
                .generate(
                    None,
                    &[],
                    &[],
                    GenerateOptions {
                        stream: false,
                        max_tokens: Some(asked),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
            assert_eq!(body["max_tokens"], json!(expected), "asked for {asked}");
            assert!(
                body.get("max_completion_tokens").is_none(),
                "the undocumented name must not reach the wire: {body}"
            );
        }
    }

    /// Thinking and effort reach the wire top-level, and no temperature is
    /// injected alongside them.
    #[tokio::test]
    async fn thinking_and_effort_reach_the_wire() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response()))
            .mount(&server)
            .await;
        let provider = provider_at(&server).await;

        let _ = provider
            .generate(
                None,
                &[],
                &[],
                GenerateOptions {
                    stream: false,
                    thinking: Some(ThinkingConfig::Enabled),
                    reasoning_effort: Some("max".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["reasoning_effort"], json!("max"));
        assert!(body.get("temperature").is_none(), "{body}");
    }

    /// Images follow the model's vision flag on the wire: `deepseek-flash`
    /// sends the `image_url` part, `deepseek-v4-pro` degrades it to the text
    /// note rather than pay for an image the model answers that it cannot see.
    #[tokio::test]
    async fn images_follow_the_models_vision_flag() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response()))
            .mount(&server)
            .await;

        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let mut message = Message::user("What colour is this?");
        message.content.push(crate::message::ContentPart::ImageUrl {
            image_url: crate::message::ImageUrl {
                url: png.to_string(),
                id: None,
            },
        });

        for (model, expect_image) in [("deepseek-flash", true), ("deepseek-v4-pro", false)] {
            let provider = DeepseekChatProvider::new(
                model,
                Some("sk-test".to_string()),
                server.uri(),
                HttpTimeouts::idle(std::time::Duration::from_secs(5)),
            )
            .unwrap();
            let _ = provider
                .generate(
                    None,
                    &[],
                    &[message.clone()],
                    GenerateOptions {
                        stream: false,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
            let parts = body["messages"][0]["content"].as_array().unwrap();
            let image = parts.iter().find(|part| part["type"] == json!("image_url"));
            assert_eq!(image.is_some(), expect_image, "{model}: {body}");
        }
    }

    /// **DeepSeek rejects a request that carries `tools` but drops a prior
    /// turn's `reasoning_content`** — so a thinking assistant message that
    /// ended in tool calls must go back out with its reasoning intact.
    #[tokio::test]
    async fn reasoning_is_echoed_when_tools_are_present() {
        use std::time::Duration;

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-test",
                "choices": [{
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })))
            .mount(&server)
            .await;
        let provider = DeepseekChatProvider::new(
            "deepseek-v4-pro",
            Some("sk-test".to_string()),
            server.uri(),
            HttpTimeouts::idle(Duration::from_secs(5)),
        )
        .unwrap();

        let tools = [Tool::new(
            "read_file",
            "Read a file",
            json!({"type": "object"}),
        )];
        let mut assistant = Message::assistant_tool_calls(vec![
            crate::message::ToolCall::new("call_1", "read_file").with_arguments("{}"),
        ]);
        assistant.content = vec![crate::message::ContentPart::Think {
            think: "first I should read it".to_string(),
        }];
        let history = vec![
            Message::user("look at x"),
            assistant,
            Message::tool_result("call_1", "contents of x"),
        ];

        let _ = provider
            .generate(
                None,
                &tools,
                &history,
                GenerateOptions {
                    stream: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(body.get("tools").is_some(), "the request carries tools");
        let messages = body["messages"].as_array().unwrap();
        let wire_assistant = &messages[1];
        assert_eq!(wire_assistant["role"], json!("assistant"));
        assert_eq!(
            wire_assistant["reasoning_content"],
            json!("first I should read it"),
            "prior reasoning must survive into the next tool-carrying request: {body}"
        );
        assert_eq!(
            wire_assistant["tool_calls"][0]["function"]["name"],
            json!("read_file")
        );
    }

    /// A 402 is a spent balance: fatal, never retried, and worded so the fix
    /// is obvious. Transient statuses keep the generic classification.
    #[tokio::test]
    async fn a_402_is_a_fatal_business_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(402).set_body_json(json!({
                "error": {
                    "message": "Insufficient Balance",
                    "type": "invalid_request_error"
                }
            })))
            .mount(&server)
            .await;
        let provider = provider_at(&server).await;

        let result = provider
            .generate(
                None,
                &[],
                &[Message::user("hi")],
                GenerateOptions {
                    stream: false,
                    ..Default::default()
                },
            )
            .await;
        let err = match result {
            Err(err) => err,
            Ok(_) => panic!("a spent balance is an error"),
        };
        let LlmError::ApiBusinessError { code, message } = err else {
            panic!("a 402 must be a business error, got {err:?}");
        };
        assert_eq!(code, "402");
        assert!(
            message.contains("insufficient DeepSeek balance"),
            "{message}"
        );
        assert!(message.contains("Insufficient Balance"), "{message}");
    }

    /// The 402 mapping leaves every other error — including retryable 429s
    /// and 5xx — exactly as the generic classifier produced it.
    #[test]
    fn only_402_is_reworded() {
        let transient = explain_insufficient_balance(LlmError::ApiStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            message: "slow down".to_string(),
            retry_hint: false,
            retry_after: None,
        });
        assert!(
            matches!(transient, LlmError::ApiStatus { .. }),
            "{transient:?}"
        );
        assert!(transient.is_retryable());

        let overflow = explain_insufficient_balance(LlmError::ApiContextOverflow);
        assert!(matches!(overflow, LlmError::ApiContextOverflow));
    }
}
