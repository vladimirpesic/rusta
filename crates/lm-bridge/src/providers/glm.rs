use std::collections::HashMap;

use serde_json::{Value, json};

use crate::{
    errors::LlmError,
    message::Message,
    provider::{ChatProvider, GenerateOptions, StreamedMessage, ThinkingConfig},
    tool::Tool,
};

use super::openai_compat::{HttpTimeouts, OpenAiCompatProvider};

/// What to do with a video on a GLM model that cannot take it.
const GLM_VIDEO_HINT: &str = "this GLM model does not take video; GLM-5.3-Flash does, or analyse \
     a local video (MP4/MOV/M4V, up to 8 MB) with the zai_vision MCP server's \
     mcp__zai_vision__analyze_video tool";

/// What the GLM Coding Plan does with a model id.
///
/// Z.AI's devpack overview says the plan serves GLM-5.3 and GLM-5.3-Flash, and
/// routes GLM-5.2/5.1 to GLM-5.3 and GLM-4.7 to GLM-5.3-Flash. The rest of this
/// table is from probing the Coding Plan endpoint, which reports the model
/// that answered in each response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingPlanModel {
    /// One of the models the plan documents.
    Documented,
    /// Accepted, and answered by this model instead.
    RoutedTo(&'static str),
    /// Answered under its own name, though the plan does not document it, so
    /// it may stop working without notice.
    Undocumented,
    /// Refused: `1311` (not in the plan) or `1113` (billed per token, outside
    /// the plan's allowance).
    NotIncluded,
    /// An id with no record here.
    Unknown,
}

/// Z.AI (Zhipu GLM) provider, for the Coding Plan and pay-per-token endpoints.
pub struct GlmChatProvider {
    inner: OpenAiCompatProvider,
    thinking_override: Option<bool>,
    /// Whether the endpoint is the Coding Plan's, which serves only
    /// [`CODING_PLAN_MODELS`](Self::CODING_PLAN_MODELS) and routes older ids
    /// onto them.
    coding_plan: bool,
}

impl GlmChatProvider {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.z.ai/api/coding/paas/v4";

    /// The models the GLM Coding Plan documents.
    pub const CODING_PLAN_MODELS: [&'static str; 2] = ["glm-5.3", "glm-5.3-flash"];

    /// Functions one request may declare. Z.AI's OpenAPI: "A max of 128
    /// functions are supported."
    pub const MAX_TOOLS: usize = 128;

    /// Z.AI documents `max_tokens` up to 128K; the coding endpoint rejects
    /// values outside `[1, 131072]`.
    pub const ZAI_MAX_TOKENS_LIMIT: u32 = 131_072;

    pub fn new(
        model: impl Into<String>,
        api_key: Option<String>,
        base_url: impl Into<String>,
        timeouts: HttpTimeouts,
    ) -> Result<Self, LlmError> {
        let model = model.into();
        let base_url = base_url.into();
        let coding_plan = Self::endpoint_is_coding_plan(&base_url).unwrap_or(false);
        // As in the Kimi provider: the catalogue knows which models are
        // multimodal, the name-based guess does not. Media follows the id in
        // the request, not the model a Coding Plan route serves it with: Z.AI
        // checks image parts against the requested id (`glm-4.7` refuses an
        // image with `1210` although GLM-5.3-Flash answers it).
        let vision = Self::supports_vision_for(&model);
        let media = Self::media_policy_for(&model);
        let mut inner = OpenAiCompatProvider::new("glm", model, base_url, api_key, timeouts)?
            .with_vision(vision)
            .with_media_policy(media)
            // Z.AI's schema has no `reasoning_details`; a stored array from a
            // gateway session must not reach it.
            .with_reasoning_details_echo(false);
        // Only send a language hint; do not emit Z.AI telemetry headers.
        let _ = inner.set_header("Accept-Language", "en-US,en");
        let thinking_override = std::env::var("ACP_GLM_THINKING")
            .ok()
            .map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"));
        Ok(Self {
            inner,
            thinking_override,
            coding_plan,
        })
    }

    /// Send `headers` with every request, below the provider's own
    /// ([`crate::custom_headers`]).
    #[must_use]
    pub fn with_custom_headers(mut self, headers: &reqwest::header::HeaderMap) -> Self {
        self.inner = self.inner.with_custom_headers(headers);
        self
    }

    /// Say whether the endpoint is the Coding Plan's, for a base URL that
    /// does not show it (a proxy). A Z.AI URL is recognised on its own.
    #[must_use]
    pub fn with_coding_plan(mut self, coding_plan: bool) -> Self {
        self.coding_plan = coding_plan;
        self
    }

    /// Whether `base_url` is a Coding Plan endpoint (`…/api/coding/paas/v4`),
    /// a pay-per-token one (`…/api/paas/v4`), or neither.
    #[must_use]
    pub fn endpoint_is_coding_plan(base_url: &str) -> Option<bool> {
        let url = base_url.trim_end_matches('/');
        if url.ends_with("/api/coding/paas/v4") {
            Some(true)
        } else if url.ends_with("/api/paas/v4") {
            Some(false)
        } else {
            None
        }
    }

    /// What the Coding Plan does with `model`. See [`CodingPlanModel`].
    #[must_use]
    pub fn coding_plan_model(model: &str) -> CodingPlanModel {
        let lower = model.to_lowercase();
        let id = lower.strip_suffix("[1m]").unwrap_or(&lower);
        match id {
            "glm-5.3" | "glm-5.3-flash" => CodingPlanModel::Documented,
            "glm-5" | "glm-5.1" | "glm-5.2" => CodingPlanModel::RoutedTo("glm-5.3"),
            "glm-4.5" | "glm-4.5-air" | "glm-4.6" | "glm-4.7" | "glm-5-turbo" => {
                CodingPlanModel::RoutedTo("glm-5.3-flash")
            }
            "glm-4.5-flash" | "glm-4.7-flash" | "glm-4.6v" | "glm-4.6v-flash" | "glm-4.5v" => {
                CodingPlanModel::Undocumented
            }
            "glm-5v-turbo"
            | "glm-5.3-flashx"
            | "glm-4.5-x"
            | "glm-4.5-airx"
            | "glm-4.7-flashx"
            | "glm-4-32b-0414-128k"
            | "glm-4.6v-flashx" => CodingPlanModel::NotIncluded,
            _ => CodingPlanModel::Unknown,
        }
    }

    /// The model that answers this provider's requests. On the Coding Plan a
    /// routed id is served by GLM-5.3 or GLM-5.3-Flash, and it takes that
    /// model's window, thinking, effort and `tool_stream` behaviour: the
    /// effort ladder of the serving model is honoured.
    fn served_model(&self) -> &str {
        let model = self.inner.model_name();
        if self.coding_plan
            && let CodingPlanModel::RoutedTo(served) = Self::coding_plan_model(model)
        {
            return served;
        }
        model
    }

    /// Reword a refusal that names a model the plan lacks, so it says what
    /// to use instead; and the empty balance a pay-per-token endpoint reports,
    /// so it says when the key is a Coding Plan's.
    fn explain_refusal(&self, error: LlmError) -> LlmError {
        let LlmError::ApiBusinessError { code, message } = error else {
            return error;
        };
        let model = self.inner.model_name();
        let use_instead = Self::CODING_PLAN_MODELS.join(" or ");
        let message = match code.as_str() {
            "1311" => format!(
                "your GLM Coding Plan does not include {model}; use {use_instead}. Z.AI: {message}"
            ),
            "1113"
                if self.coding_plan
                    && Self::coding_plan_model(model) == CodingPlanModel::NotIncluded =>
            {
                format!(
                    "{model} is not part of the GLM Coding Plan, so Z.AI bills it per token, and \
                     the account has no balance for it; use {use_instead}. Z.AI: {message}"
                )
            }
            // Z.AI bills a key by its type, but an older key made for an
            // account with a Coding Plan ("Compatible") is billed by the
            // endpoint: here, against the account balance.
            "1113" if !self.coding_plan => format!(
                "Z.AI bills this key per token on this endpoint, and the account has no \
                 balance. If it is a GLM Coding Plan key made before 2026-10-16, it uses the \
                 plan only on the Coding Plan endpoint: use --billing subscription. Otherwise \
                 top up the account. Z.AI: {message}"
            ),
            _ => message,
        };
        LlmError::ApiBusinessError { code, message }
    }

    pub fn model_metadata() -> &'static HashMap<&'static str, u64> {
        use std::sync::LazyLock;
        static METADATA: LazyLock<HashMap<&str, u64>> = LazyLock::new(|| {
            [
                ("glm-5", 200_000u64),
                ("glm-5.1", 200_000),
                ("glm-5-turbo", 200_000),
                ("glm-5v-turbo", 200_000),
                ("glm-4.7", 200_000),
                ("glm-4.7-flash", 200_000),
                ("glm-4.7-flashx", 200_000),
                ("glm-4.6", 200_000),
                ("glm-4.5", 128_000),
                ("glm-4.5-air", 128_000),
                ("glm-4.5-x", 128_000),
                ("glm-4.5-airx", 128_000),
                ("glm-4.5-flash", 128_000),
                ("glm-4-32b-0414-128k", 128_000),
                ("glm-4.6v", 128_000),
                ("glm-4.6v-flash", 128_000),
                ("glm-4.6v-flashx", 128_000),
                ("glm-4.5v", 64_000),
                ("glm-5.2", 1_048_576),
                ("glm-5.3", 1_048_576),
                ("glm-5.3-flash", 1_048_576),
                ("glm-5.3-flashx", 1_048_576),
            ]
            .into_iter()
            .collect()
        });
        &METADATA
    }

    pub fn context_window(model: &str) -> Option<u64> {
        // Exact entry, then the explicit 1M marker, then the nearest lower
        // catalogued minor — so a `glm-5.4` this table does not list yet
        // inherits `glm-5.3`'s 1M window instead of the 128k default.
        if let Some(window) = Self::model_metadata().get(model).copied() {
            return Some(window);
        }
        if model.contains("[1m]") {
            return Some(1_048_576);
        }
        crate::model_version::lookup_with_minor_fallback(model, Self::model_metadata())
    }

    fn build_extra_body(
        options: &GenerateOptions,
        model: &str,
        thinking_override: Option<bool>,
        has_tools: bool,
    ) -> Value {
        let mut extra = json!({});

        // ACP_GLM_THINKING env override takes precedence over the model heuristic.
        let requested_thinking = match thinking_override {
            Some(enabled) => enabled,
            None => {
                Self::supports_thinking(model)
                    && !matches!(options.thinking, Some(ThinkingConfig::Disabled))
            }
        };

        // GLM-5.3 removed the off switch: `thinking: {type: "disabled"}` is
        // rejected outright, and z.ai's migration guide says to send `enabled`
        // with `reasoning_effort: "low"` instead. So a caller asking for "no
        // thinking" gets the lightest thinking the model still offers rather
        // than a 400 that reads like a malformed request body.
        let forced_on = Self::thinking_is_mandatory(model);
        let thinking_enabled = requested_thinking || forced_on;
        let downgraded_to_low = forced_on && !requested_thinking;

        if thinking_enabled {
            let mut thinking = json!({"type": "enabled"});
            // Preserved Thinking. Z.AI spells this `clear_thinking`, and the
            // polarity is inverted: `true` wipes the reasoning_content of prior
            // assistant turns, `false` keeps it. Omitting the field takes the
            // endpoint's default — the Coding Plan endpoint preserves, the
            // standard API endpoint clears — which is why `None` sends nothing.
            //
            // Preserving only works because `openai_common` replays each
            // `ContentPart::Think` back as the assistant message's
            // `reasoning_content`, exactly as z.ai requires.
            if let Some(keep) = options.thinking_keep {
                thinking["clear_thinking"] = json!(!keep);
            }
            extra["thinking"] = thinking;
        } else if Self::supports_thinking(model) {
            // `thinking.type` defaults to "enabled" server-side, so a thinking
            // model must be told "disabled" explicitly. Omitting the field
            // would silently leave reasoning switched on.
            extra["thinking"] = json!({"type": "disabled"});
        }

        // `reasoning_effort` is accepted by GLM-5.2 and newer only, and only
        // makes sense while thinking is on. Sending it to an older model is at
        // best ignored; sending it alongside `thinking: {type: disabled}`
        // contradicts the request and can leak the field to a model that has
        // reasoning switched off — so gate on both.
        if thinking_enabled && Self::supports_reasoning_effort(model) {
            // A caller who asked for thinking *off* on a model that cannot
            // switch it off gets the floor of the ladder, not their configured
            // effort — otherwise "thinking = false" would silently run `max`.
            let requested = if downgraded_to_low {
                Some("low")
            } else {
                options.reasoning_effort.as_deref()
            };
            if let Some(effort) = requested {
                extra["reasoning_effort"] = json!(Self::fold_reasoning_effort(model, effort));
            }
        }

        // Tool-call streaming. Without it z.ai buffers the whole `arguments`
        // string and emits it in one delta, so a long edit payload arrives as a
        // single stall; with it the arguments stream in fragments that
        // `chat_completions_stream` already concatenates. Only meaningful when
        // there is both a stream to fill and a tool to call.
        if options.stream && has_tools && Self::supports_tool_stream(model) {
            extra["tool_stream"] = json!(true);
        }

        extra
    }

    /// Whether `model` refuses `thinking: {type: "disabled"}`: GLM-5.3 and newer.
    ///
    /// z.ai: "Disabling reasoning is no longer supported." The replacement is
    /// `thinking: {type: "enabled"}` with `reasoning_effort: "low"`.
    fn thinking_is_mandatory(model: &str) -> bool {
        Self::at_least(model, 5, 3)
    }

    /// Whether `model` accepts `tool_stream`: GLM-4.6 and newer.
    ///
    /// z.ai documents the parameter for the `glm-4.6`, `glm-4.7` and `glm-5`
    /// lines. The unversioned `glm-5*` aliases (`glm-5-turbo`, `glm-5v-turbo`)
    /// belong to that generation and are included; anything older is not.
    fn supports_tool_stream(model: &str) -> bool {
        let lower = model.to_lowercase();
        if crate::model_version::parse(&lower).is_none() {
            // No `<major>.<minor>` to compare — accept the glm-5 family by name.
            return lower.starts_with("glm-5");
        }
        Self::at_least(model, 4, 6)
    }

    /// Fold a request onto the effort ladder `model` actually implements.
    ///
    /// Rusta exposes z.ai's full seven-rung scale, but GLM-5.3 narrowed its
    /// own ladder to `low` / `high` / `max`. Sending `medium` there is a 400, so
    /// each rung is mapped onto the nearest one the model still serves rather
    /// than dropped — dropping it would silently fall back to `max`, the most
    /// expensive setting, which is the opposite of what a caller asking for
    /// `minimal` wants.
    fn fold_reasoning_effort<'a>(model: &str, effort: &'a str) -> &'a str {
        if !Self::thinking_is_mandatory(model) {
            return effort;
        }
        match effort {
            "none" | "minimal" | "low" => "low",
            "medium" | "high" => "high",
            "xhigh" | "max" => "max",
            // An unrecognised rung is passed through untouched: z.ai owns the
            // vocabulary, and a value added after this build should reach it.
            other => other,
        }
    }

    /// Whether `model` is a GLM at or past `major.minor`.
    ///
    /// An id carrying no minor version (`glm-5-turbo`) names no point on the
    /// ladder and is never "at least" anything.
    fn at_least(model: &str, major: u32, minor: u32) -> bool {
        let lower = model.to_lowercase();
        let Some(version) = crate::model_version::parse(&lower) else {
            return false;
        };
        if version.prefix != "glm-" {
            return false;
        }
        match version.major.parse::<u32>() {
            Ok(m) => m > major || (m == major && version.minor >= minor),
            Err(_) => false,
        }
    }

    /// Whether `model` accepts `reasoning_effort`: GLM-5.2 and newer.
    ///
    /// A model id with no `<major>.<minor>` version (`glm-5-turbo`) names no
    /// minor, and no such model supports the parameter.
    fn supports_reasoning_effort(model: &str) -> bool {
        Self::at_least(model, 5, 2)
    }

    /// Clamp sampling to the ranges z.ai documents for chat completions:
    /// `temperature` in `[0.0, 1.0]`, `top_p` in `[0.01, 1.0]`.
    ///
    /// Both bounds are inclusive in the current API — the older open-interval
    /// rule inherited from the Python SDK rewrote a deliberate `1.0` as `0.99`,
    /// which is a different distribution than the one asked for and, on GLM-5.3,
    /// a different one than the model's own default of `1.0`.
    ///
    /// A non-positive temperature still means greedy decoding, which z.ai spells
    /// `do_sample: false` rather than `temperature: 0`.
    fn sanitize_options(options: &mut GenerateOptions) {
        if let Some(t) = options.temperature {
            if t <= 0.0 {
                options.temperature = Some(0.0);
                options.do_sample = Some(false);
            } else if t > 1.0 {
                options.temperature = Some(1.0);
            }
        }

        if let Some(p) = options.top_p {
            if p < 0.01 {
                options.top_p = Some(0.01);
            } else if p > 1.0 {
                options.top_p = Some(1.0);
            }
        }
    }

    fn supports_thinking(model: &str) -> bool {
        let lower = model.to_lowercase();
        lower.starts_with("glm-4.5")
            || lower.starts_with("glm-4.6")
            || lower.starts_with("glm-4.7")
            || lower.starts_with("glm-5")
    }

    pub fn context_window_for(model: &str) -> u64 {
        Self::context_window(model).unwrap_or(128_000)
    }

    /// Whether `model` accepts image content parts: GLM-5.3-Flash/FlashX and
    /// the `v` vision line (`glm-5v-turbo`, `glm-4.6v*`, `glm-4.5v`), per
    /// Z.AI's OpenAPI vision request variant. Text models such as `glm-5.3`
    /// refuse any image part with `1210`.
    pub fn supports_vision_for(model: &str) -> bool {
        let lower = model.to_lowercase();
        ["glm-5.3-flash", "glm-5v", "glm-4.6v", "glm-4.5v"]
            .iter()
            .any(|line| lower.starts_with(line))
    }

    /// Whether `model` takes video. Z.AI documents video for its vision models
    /// as a URL; an inline `data:` video was verified on GLM-5.3-Flash, so
    /// only that line is sent video inline.
    pub fn supports_video_for(model: &str) -> bool {
        model.to_lowercase().starts_with("glm-5.3-flash")
    }

    /// Which media `model` takes: Z.AI's image formats on vision models, and
    /// inline video where it was verified.
    pub fn media_policy_for(model: &str) -> crate::media::MediaPolicy {
        use crate::media::{GLM_IMAGE_MIMES, MediaPolicy, VideoSupport};
        if !Self::supports_vision_for(model) {
            return MediaPolicy {
                image_mimes: &[],
                video: VideoSupport::Unsupported {
                    hint: GLM_VIDEO_HINT,
                },
                max_image_edge_px: None,
            };
        }
        MediaPolicy {
            image_mimes: GLM_IMAGE_MIMES,
            video: if Self::supports_video_for(model) {
                VideoSupport::Inline
            } else {
                VideoSupport::Unsupported {
                    hint: GLM_VIDEO_HINT,
                }
            },
            max_image_edge_px: None,
        }
    }

    fn known_models_static() -> Vec<crate::provider::ModelInfo> {
        use crate::provider::ModelInfo;
        [
            ("glm-5.3", 1_048_576u64),
            ("glm-5.3-flash", 1_048_576),
            ("glm-5.3-flashx", 1_048_576),
            ("glm-5.2", 1_048_576),
            ("glm-5.1", 200_000),
            ("glm-5", 200_000),
            ("glm-5-turbo", 200_000),
            ("glm-5v-turbo", 200_000),
            ("glm-4.7", 200_000),
            ("glm-4.7-flash", 200_000),
            ("glm-4.7-flashx", 200_000),
            ("glm-4.6", 200_000),
            ("glm-4.5", 128_000),
            ("glm-4.5-air", 128_000),
            ("glm-4.5-x", 128_000),
            ("glm-4.5-airx", 128_000),
            ("glm-4.5-flash", 128_000),
            ("glm-4-32b-0414-128k", 128_000),
            ("glm-4.6v", 128_000),
            ("glm-4.6v-flash", 128_000),
            ("glm-4.6v-flashx", 128_000),
            ("glm-4.5v", 64_000),
            ("glm-5.1[1m]", 1_048_576),
            ("glm-5v-turbo[1m]", 1_048_576),
            ("glm-5.2[1m]", 1_048_576),
        ]
        .into_iter()
        .map(|(id, window)| ModelInfo {
            id: id.to_string(),
            display_name: None,
            // Derived, never hand-written; see `KimiChatProvider`.
            supports_vision: Self::supports_vision_for(id),
            context_window: window,
        })
        .collect()
    }

    /// The models the Coding Plan documents, as [`known_models`] lists them.
    ///
    /// [`known_models`]: ChatProvider::known_models
    fn coding_plan_models() -> Vec<crate::provider::ModelInfo> {
        Self::CODING_PLAN_MODELS
            .iter()
            .map(|id| crate::provider::ModelInfo {
                id: (*id).to_string(),
                display_name: None,
                supports_vision: Self::supports_vision_for(id),
                context_window: Self::context_window_for(id),
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl ChatProvider for GlmChatProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn context_window(&self) -> u64 {
        Self::context_window_for(self.served_model())
    }

    fn supports_vision(&self) -> bool {
        Self::supports_vision_for(self.model_name())
    }

    fn media_policy(&self) -> crate::media::MediaPolicy {
        Self::media_policy_for(self.model_name())
    }

    fn known_models(&self) -> Vec<crate::provider::ModelInfo> {
        if self.coding_plan {
            Self::coding_plan_models()
        } else {
            Self::known_models_static()
        }
    }

    fn max_tools(&self) -> Option<usize> {
        Some(Self::MAX_TOOLS)
    }

    /// Z.AI documents output up to 128K, so an explicit cap is sent even when
    /// none is configured, rather than relying on the endpoint's undocumented
    /// default; generic endpoints stay uncapped. An unknown window sends
    /// nothing.
    fn completion_tokens_when_unset(&self, input_tokens: u64, context_window: u64) -> Option<u32> {
        if context_window == 0 {
            return None;
        }
        let room = context_window
            .saturating_sub(input_tokens)
            .min(u64::from(Self::ZAI_MAX_TOKENS_LIMIT));
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
        Self::sanitize_options(&mut options);
        let mut extra_body = Self::build_extra_body(
            &options,
            self.served_model(),
            self.thinking_override,
            !tools.is_empty(),
        );

        // Z.AI expects `max_tokens`, not the OpenAI-compatible `max_completion_tokens`.
        if let Some(max) = options.max_completion_tokens.or(options.max_tokens) {
            let clamped = max.clamp(1, Self::ZAI_MAX_TOKENS_LIMIT);
            extra_body["max_tokens"] = json!(clamped);
            options.max_completion_tokens = None;
            options.max_tokens = None;
        }

        self.inner
            .generate(system_prompt, tools, history, options, extra_body)
            .await
            .map_err(|e| self.explain_refusal(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_window_known_models() {
        assert_eq!(GlmChatProvider::context_window_for("glm-5.1"), 200_000);
        assert_eq!(GlmChatProvider::context_window_for("glm-5v-turbo"), 200_000);
    }

    /// With no cap configured, GLM still sends one, clamped to the documented
    /// 128K output limit; an unknown window sends nothing.
    #[test]
    fn test_unset_completion_tokens_clamps_to_the_output_limit() {
        let provider = GlmChatProvider::new(
            "glm-5.1",
            None,
            GlmChatProvider::DEFAULT_BASE_URL,
            HttpTimeouts::idle(std::time::Duration::from_secs(5)),
        )
        .unwrap();
        // Room left in the window, below the cap.
        assert_eq!(
            provider.completion_tokens_when_unset(100_000, 200_000),
            Some(100_000)
        );
        // Room left above the cap -> the cap.
        assert_eq!(
            provider.completion_tokens_when_unset(0, 200_000),
            Some(131_072)
        );
        assert_eq!(provider.completion_tokens_when_unset(0, 0), None);
    }

    #[test]
    fn test_context_window_one_million_variant() {
        assert_eq!(
            GlmChatProvider::context_window_for("glm-5.1[1m]"),
            1_048_576
        );
    }

    #[test]
    fn test_context_window_unknown_defaults_to_128k() {
        assert_eq!(GlmChatProvider::context_window_for("glm-unknown"), 128_000);
    }

    #[test]
    fn test_sanitize_options_clamps_temperature_and_sets_do_sample() {
        let mut options = GenerateOptions {
            temperature: Some(0.0),
            top_p: Some(1.5),
            ..Default::default()
        };
        GlmChatProvider::sanitize_options(&mut options);
        assert_eq!(options.temperature, Some(0.0));
        assert_eq!(options.do_sample, Some(false));
        assert_eq!(options.top_p, Some(1.0));
    }

    /// `thinking.type` defaults to "enabled" server-side, so switching thinking
    /// off means saying so: omitting the field leaves reasoning silently
    /// switched on.
    #[test]
    fn test_build_extra_body_respects_thinking_override() {
        let options = GenerateOptions::default();
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.1", Some(false), false);
        assert_eq!(extra["thinking"], json!({"type": "disabled"}));
    }

    /// Disabling via `ThinkingConfig` must also be explicit.
    #[test]
    fn explicitly_disabled_thinking_is_sent_as_disabled() {
        let options = GenerateOptions {
            thinking: Some(ThinkingConfig::Disabled),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.2", None, false);
        assert_eq!(extra["thinking"], json!({"type": "disabled"}));
    }

    /// With thinking explicitly disabled, no `reasoning_effort` is emitted even
    /// when one is configured — an off request must not carry the field.
    #[test]
    fn disabled_thinking_suppresses_reasoning_effort() {
        let options = GenerateOptions {
            thinking: Some(ThinkingConfig::Disabled),
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.2", None, false);
        assert_eq!(extra["thinking"], json!({"type": "disabled"}));
        assert!(extra.get("reasoning_effort").is_none());
    }

    /// The ACP thinking-off override suppresses effort the same way.
    #[test]
    fn thinking_override_off_suppresses_reasoning_effort() {
        let options = GenerateOptions {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.2", Some(false), false);
        assert!(extra.get("reasoning_effort").is_none());
    }

    /// A model that cannot think gets no `thinking` field at all.
    #[test]
    fn non_thinking_model_gets_no_thinking_field() {
        let options = GenerateOptions {
            thinking: Some(ThinkingConfig::Disabled),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-3-legacy", None, false);
        assert!(extra.get("thinking").is_none());
    }

    /// Preserved Thinking is `clear_thinking`, and its polarity is inverted:
    /// keeping reasoning means *not* clearing it. z.ai has no `keep` field.
    #[test]
    fn test_build_extra_body_keeps_thinking_when_requested() {
        let options = GenerateOptions {
            thinking_keep: Some(true),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.1", None, false);
        assert_eq!(extra["thinking"]["type"], json!("enabled"));
        assert_eq!(extra["thinking"]["clear_thinking"], json!(false));
        assert!(
            extra["thinking"].get("keep").is_none(),
            "`keep` is a Kimi field; z.ai does not accept it"
        );
    }

    /// ...and asking not to keep it clears it.
    #[test]
    fn thinking_keep_false_clears_prior_reasoning() {
        let options = GenerateOptions {
            thinking_keep: Some(false),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.2", None, false);
        assert_eq!(extra["thinking"]["clear_thinking"], json!(true));
    }

    #[test]
    fn test_build_extra_body_includes_reasoning_effort() {
        let options = GenerateOptions {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.2", None, false);
        assert_eq!(extra["reasoning_effort"], json!("high"));
    }

    #[test]
    fn test_context_window_for_glm_5_2() {
        assert_eq!(GlmChatProvider::context_window_for("glm-5.2"), 1_048_576);
    }

    /// A minor this table does not list yet must inherit its predecessor's
    /// window, not silently drop to the 128k default and make the agent compact
    /// its context eight times more often than it needs to.
    #[test]
    fn uncatalogued_minor_inherits_the_previous_minors_window() {
        assert_eq!(GlmChatProvider::context_window("glm-5.4"), Some(1_048_576));
        assert_eq!(GlmChatProvider::context_window("glm-4.9"), Some(200_000));
        assert_eq!(GlmChatProvider::context_window_for("glm-5.4"), 1_048_576);
    }

    /// Every model GLM advertises must resolve to the window it advertises.
    /// This pins the whole catalogue — including the `[1m]` variants, which are
    /// resolved by the marker heuristic rather than the metadata table — so the
    /// minor-version fallback cannot silently re-map a shipping model.
    #[test]
    fn every_advertised_glm_model_resolves_to_its_advertised_window() {
        for info in GlmChatProvider::known_models_static() {
            assert_eq!(
                GlmChatProvider::context_window_for(&info.id),
                info.context_window,
                "{} must resolve to its advertised window",
                info.id
            );
        }
    }

    /// The 1M marker is checked before the minor fallback, so an uncatalogued
    /// 1M variant still resolves to 1M rather than inheriting a smaller minor.
    #[test]
    fn one_million_marker_beats_the_minor_fallback() {
        assert_eq!(
            GlmChatProvider::context_window("glm-5.1[1m]"),
            Some(1_048_576)
        );
        assert_eq!(
            GlmChatProvider::context_window("glm-4.6[1m]"),
            Some(1_048_576)
        );
        assert_eq!(
            GlmChatProvider::context_window("glm-5.9[1m]"),
            Some(1_048_576)
        );
    }

    /// A `.0` minor has nothing below it and must not climb to `.1`.
    #[test]
    fn glm_zero_minor_does_not_climb() {
        assert_eq!(GlmChatProvider::context_window("glm-5.0"), None);
        assert_eq!(GlmChatProvider::context_window_for("glm-5.0"), 128_000);
    }

    /// Rusta must not decide Preserved Thinking for GLM. When nobody asks,
    /// `clear_thinking` is omitted and the *endpoint* decides: the Coding Plan
    /// endpoint preserves reasoning, the standard API endpoint clears it.
    #[test]
    fn preserved_thinking_is_left_to_the_endpoint_default_for_glm() {
        let options = GenerateOptions {
            thinking: Some(ThinkingConfig::Enabled),
            thinking_keep: None,
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.2", None, false);
        assert_eq!(extra["thinking"]["type"], json!("enabled"));
        assert!(
            extra["thinking"].get("clear_thinking").is_none(),
            "GLM must not inherit the Kimi Preserved Thinking default"
        );
        assert!(extra["thinking"].get("keep").is_none());
    }

    /// ...and an explicit request still turns it on for GLM.
    #[test]
    fn glm_still_honours_an_explicit_thinking_keep() {
        let options = GenerateOptions {
            thinking: Some(ThinkingConfig::Enabled),
            thinking_keep: Some(true),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.2", None, false);
        assert_eq!(extra["thinking"]["clear_thinking"], json!(false));
    }

    /// `reasoning_effort` is a GLM-5.2-and-newer parameter. Older models must
    /// not be sent a field they do not accept.
    #[test]
    fn reasoning_effort_is_gated_to_glm_5_2_and_newer() {
        for model in ["glm-5.2", "glm-5.3", "glm-5.2[1m]", "glm-6.0"] {
            assert!(
                GlmChatProvider::supports_reasoning_effort(model),
                "{model} must accept reasoning_effort"
            );
        }
        for model in [
            "glm-5.1",
            "glm-5.0",
            "glm-4.7",
            "glm-5-turbo",
            "glm-5v-turbo",
            "glm-4.5-air",
            "",
        ] {
            assert!(
                !GlmChatProvider::supports_reasoning_effort(model),
                "{model} must not be sent reasoning_effort"
            );
        }
    }

    #[test]
    fn reasoning_effort_is_dropped_for_older_models() {
        let options = GenerateOptions {
            reasoning_effort: Some("high".to_string()),
            ..Default::default()
        };
        let old = GlmChatProvider::build_extra_body(&options, "glm-5.1", None, false);
        assert!(old.get("reasoning_effort").is_none());

        let new = GlmChatProvider::build_extra_body(&options, "glm-5.2", None, false);
        assert_eq!(new["reasoning_effort"], json!("high"));
    }

    /// Every value z.ai documents must survive the passthrough unaltered.
    #[test]
    fn every_documented_reasoning_effort_value_passes_through() {
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let options = GenerateOptions {
                reasoning_effort: Some(effort.to_string()),
                ..Default::default()
            };
            let extra = GlmChatProvider::build_extra_body(&options, "glm-5.2", None, false);
            assert_eq!(extra["reasoning_effort"], json!(effort));
        }
    }

    /// The advertised catalogue and the resolver must never disagree.
    #[test]
    fn advertised_vision_flags_agree_with_the_resolver() {
        for info in GlmChatProvider::known_models_static() {
            assert_eq!(
                info.supports_vision,
                GlmChatProvider::supports_vision_for(&info.id),
                "{} advertises a vision flag the resolver disagrees with",
                info.id
            );
        }
    }

    /// GLM-5.3-Flash and the `v` line take images; the text models do not.
    #[test]
    fn glm_vision_follows_the_vision_line() {
        for model in [
            "glm-5.3-flash",
            "glm-5.3-flashx",
            "glm-5v-turbo",
            "glm-5v-turbo[1m]",
            "glm-4.6v",
            "glm-4.6v-flash",
            "glm-4.6v-flashx",
            "glm-4.5v",
        ] {
            assert!(GlmChatProvider::supports_vision_for(model), "{model}");
        }
        for model in [
            "glm-5.3",
            "glm-5.2",
            "glm-5.1",
            "glm-4.7",
            "glm-4.7-flash",
            "glm-5-turbo",
            "",
        ] {
            assert!(!GlmChatProvider::supports_vision_for(model), "{model}");
        }
    }

    /// The fallback must not invent a window for an unrelated model.
    #[test]
    fn unknown_model_still_falls_back_to_the_default_window() {
        assert_eq!(GlmChatProvider::context_window("glm-9.9"), None);
        assert_eq!(GlmChatProvider::context_window("llama-3.1"), None);
        assert_eq!(GlmChatProvider::context_window_for("llama-3.1"), 128_000);
        assert_eq!(
            GlmChatProvider::context_window_for("glm-5.2[1m]"),
            1_048_576
        );
    }

    // ---- GLM-5.3 ----------------------------------------------------------

    /// GLM-5.3 rejects `thinking: {type: "disabled"}` outright. Every route that
    /// switches thinking off sends `enabled` instead, or the request fails
    /// before the model sees it.
    #[test]
    fn glm_5_3_never_sends_disabled_thinking() {
        let explicit = GenerateOptions {
            thinking: Some(ThinkingConfig::Disabled),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&explicit, "glm-5.3", None, false);
        assert_eq!(extra["thinking"]["type"], json!("enabled"));

        let extra = GlmChatProvider::build_extra_body(
            &GenerateOptions::default(),
            "glm-5.3",
            Some(false),
            false,
        );
        assert_eq!(extra["thinking"]["type"], json!("enabled"));
    }

    /// ...and "off" becomes the floor of the ladder, not the configured effort.
    /// Silently running `max` for a caller who asked for no thinking at all
    /// would be the most expensive possible reading of the request.
    #[test]
    fn thinking_off_on_glm_5_3_downgrades_to_low_effort() {
        let options = GenerateOptions {
            thinking: Some(ThinkingConfig::Disabled),
            reasoning_effort: Some("max".to_string()),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.3", None, false);
        assert_eq!(extra["thinking"]["type"], json!("enabled"));
        assert_eq!(extra["reasoning_effort"], json!("low"));
    }

    /// Thinking stays switchable on everything before 5.3.
    #[test]
    fn thinking_is_still_optional_below_glm_5_3() {
        for model in ["glm-5.2", "glm-5.1", "glm-4.7", "glm-4.6"] {
            let options = GenerateOptions {
                thinking: Some(ThinkingConfig::Disabled),
                ..Default::default()
            };
            let extra = GlmChatProvider::build_extra_body(&options, model, None, false);
            assert_eq!(
                extra["thinking"],
                json!({"type": "disabled"}),
                "{model} must still accept an explicit thinking-off"
            );
        }
        assert!(!GlmChatProvider::thinking_is_mandatory("glm-5.2"));
        for model in ["glm-5.3", "glm-5.4", "glm-6.0"] {
            assert!(GlmChatProvider::thinking_is_mandatory(model), "{model}");
        }
    }

    /// GLM-5.3 narrowed its effort ladder to low/high/max. Rusta still exposes
    /// z.ai's full seven-rung vocabulary, so each rung folds onto the nearest one
    /// 5.3 serves — `medium` would otherwise be a 400.
    #[test]
    fn reasoning_effort_folds_onto_the_glm_5_3_ladder() {
        for (asked, sent) in [
            ("none", "low"),
            ("minimal", "low"),
            ("low", "low"),
            ("medium", "high"),
            ("high", "high"),
            ("xhigh", "max"),
            ("max", "max"),
        ] {
            let options = GenerateOptions {
                reasoning_effort: Some(asked.to_string()),
                ..Default::default()
            };
            let extra = GlmChatProvider::build_extra_body(&options, "glm-5.3", None, false);
            assert_eq!(
                extra["reasoning_effort"],
                json!(sent),
                "glm-5.3 must receive `{sent}` when asked for `{asked}`"
            );
        }
    }

    /// The fold is 5.3-and-newer only: GLM-5.2 documents all seven rungs and must
    /// keep receiving the one it was given.
    #[test]
    fn reasoning_effort_is_not_folded_for_glm_5_2() {
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            let options = GenerateOptions {
                reasoning_effort: Some(effort.to_string()),
                ..Default::default()
            };
            let extra = GlmChatProvider::build_extra_body(&options, "glm-5.2", None, false);
            assert_eq!(extra["reasoning_effort"], json!(effort));
        }
    }

    /// An effort value z.ai adds after this build must reach it untouched — the
    /// vocabulary is theirs, and folding an unknown rung would invent a meaning.
    #[test]
    fn an_unrecognised_effort_rung_is_passed_through() {
        assert_eq!(
            GlmChatProvider::fold_reasoning_effort("glm-5.3", "ultra"),
            "ultra"
        );
    }

    #[test]
    fn glm_5_3_is_a_one_million_context_text_only_model() {
        assert_eq!(GlmChatProvider::context_window_for("glm-5.3"), 1_048_576);
        assert!(!GlmChatProvider::supports_vision_for("glm-5.3"));
    }

    // ---- GLM-5.3-Flash ----------------------------------------------------

    /// GLM-5.3-Flash is a GLM-5.3 in every text respect — 1M window, thinking
    /// that cannot be switched off, the `low`/`high`/`max` ladder,
    /// `tool_stream` — and it also takes images and inline video.
    #[test]
    fn glm_5_3_flash_is_a_one_million_context_vision_model() {
        for model in ["glm-5.3-flash", "glm-5.3-flashx"] {
            assert_eq!(GlmChatProvider::context_window_for(model), 1_048_576);
            assert!(GlmChatProvider::supports_vision_for(model));
            let policy = GlmChatProvider::media_policy_for(model);
            assert!(policy.accepts_image("image/webp"), "{model}");
            assert!(!policy.accepts_image("image/bmp"), "{model}");
            assert!(policy.takes_video(), "{model}");
            assert!(GlmChatProvider::thinking_is_mandatory(model));
            assert!(GlmChatProvider::supports_tool_stream(model));
        }

        let off = GenerateOptions {
            thinking: Some(ThinkingConfig::Disabled),
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&off, "glm-5.3-flash", None, false);
        assert_eq!(extra["thinking"]["type"], json!("enabled"));
        assert_eq!(extra["reasoning_effort"], json!("low"));

        let medium = GenerateOptions {
            reasoning_effort: Some("medium".to_string()),
            stream: true,
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&medium, "glm-5.3-flash", None, true);
        assert_eq!(extra["reasoning_effort"], json!("high"));
        assert_eq!(extra["tool_stream"], json!(true));
    }

    // ---- Coding Plan ------------------------------------------------------

    fn provider_at(model: &str, base_url: &str) -> GlmChatProvider {
        GlmChatProvider::new(
            model,
            Some("sk-test".to_string()),
            base_url,
            HttpTimeouts::idle(std::time::Duration::from_secs(5)),
        )
        .unwrap()
    }

    #[test]
    fn the_endpoint_says_which_plan_it_serves() {
        for url in [
            "https://api.z.ai/api/coding/paas/v4",
            "https://open.bigmodel.cn/api/coding/paas/v4/",
        ] {
            assert_eq!(GlmChatProvider::endpoint_is_coding_plan(url), Some(true));
        }
        assert_eq!(
            GlmChatProvider::endpoint_is_coding_plan("https://api.z.ai/api/paas/v4"),
            Some(false)
        );
        assert_eq!(
            GlmChatProvider::endpoint_is_coding_plan("http://localhost:8080/v1"),
            None
        );
    }

    #[test]
    fn the_coding_plan_table_matches_the_live_probe() {
        use CodingPlanModel::*;
        for (model, expected) in [
            ("glm-5.3", Documented),
            ("glm-5.3-flash", Documented),
            ("GLM-5.3-Flash", Documented),
            ("glm-5.1", RoutedTo("glm-5.3")),
            ("glm-5.2[1m]", RoutedTo("glm-5.3")),
            ("glm-5", RoutedTo("glm-5.3")),
            ("glm-4.7", RoutedTo("glm-5.3-flash")),
            ("glm-5-turbo", RoutedTo("glm-5.3-flash")),
            ("glm-4.5-air", RoutedTo("glm-5.3-flash")),
            ("glm-4.6v", Undocumented),
            ("glm-4.7-flash", Undocumented),
            ("glm-5v-turbo", NotIncluded),
            ("glm-5.3-flashx", NotIncluded),
            ("glm-4.5-x", NotIncluded),
            ("glm-9", Unknown),
        ] {
            assert_eq!(
                GlmChatProvider::coding_plan_model(model),
                expected,
                "{model}"
            );
        }
        for id in GlmChatProvider::CODING_PLAN_MODELS {
            assert_eq!(GlmChatProvider::coding_plan_model(id), Documented);
        }
    }

    /// On the Coding Plan a routed id is answered by GLM-5.3 or GLM-5.3-Flash
    /// and takes that model's window; on the pay-per-token endpoint it is its
    /// own model. Media follows the requested id either way.
    #[test]
    fn a_routed_id_takes_the_serving_models_window_on_the_coding_plan() {
        let coding = provider_at("glm-5.1", "https://api.z.ai/api/coding/paas/v4");
        assert_eq!(coding.context_window(), 1_048_576);
        let api = provider_at("glm-5.1", "https://api.z.ai/api/paas/v4");
        assert_eq!(api.context_window(), 200_000);

        let flash_routed = provider_at("glm-4.7", "https://api.z.ai/api/coding/paas/v4");
        assert_eq!(flash_routed.context_window(), 1_048_576);
        assert!(
            !flash_routed.supports_vision(),
            "Z.AI checks images against the requested id"
        );

        let proxy = provider_at("glm-5.1", "http://localhost:8080/v1");
        assert_eq!(proxy.context_window(), 200_000);
        assert_eq!(proxy.with_coding_plan(true).context_window(), 1_048_576);
    }

    /// The Coding Plan lists the two models it documents; the pay-per-token
    /// endpoint lists the whole catalogue.
    #[test]
    fn the_coding_plan_lists_only_its_documented_models() {
        let coding = provider_at("glm-5.3", "https://api.z.ai/api/coding/paas/v4");
        let ids: Vec<String> = coding.known_models().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["glm-5.3", "glm-5.3-flash"]);
        let flash = &coding.known_models()[1];
        assert!(flash.supports_vision);
        assert_eq!(flash.context_window, 1_048_576);

        let api = provider_at("glm-5.3", "https://api.z.ai/api/paas/v4");
        assert!(api.known_models().len() > 10);
    }

    /// Custom headers reach the wire, below the provider's own.
    #[tokio::test]
    async fn custom_headers_are_sent_below_the_providers_own() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response()))
            .mount(&server)
            .await;
        let headers =
            crate::custom_headers::header_map([("X-Route", "blue"), ("Accept-Language", "de")])
                .unwrap();
        let provider = provider_at("glm-5.3", &server.uri()).with_custom_headers(&headers);
        provider
            .generate(
                None,
                &[],
                &[Message::user("hi")],
                GenerateOptions {
                    stream: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let sent = &requests[0].headers;
        assert_eq!(sent["x-route"], "blue");
        assert_eq!(
            sent["accept-language"], "en-US,en",
            "the provider's own header keeps its value"
        );
    }

    /// Z.AI declares at most 128 functions per request, on either endpoint.
    #[test]
    fn glm_caps_the_tool_table_at_128() {
        for url in [
            "https://api.z.ai/api/coding/paas/v4",
            "https://api.z.ai/api/paas/v4",
        ] {
            assert_eq!(provider_at("glm-5.3", url).max_tools(), Some(128));
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

    /// A routed id is sent as the serving model's request: GLM-5.1 on the
    /// Coding Plan is answered by GLM-5.3, so "thinking off" becomes the
    /// lowest effort and the effort folds onto GLM-5.3's ladder.
    #[tokio::test]
    async fn a_routed_id_gets_the_serving_models_thinking() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response()))
            .mount(&server)
            .await;
        let provider = provider_at("glm-5.1", &server.uri()).with_coding_plan(true);
        provider
            .generate(
                None,
                &[],
                &[Message::user("hi")],
                GenerateOptions {
                    stream: false,
                    thinking: Some(ThinkingConfig::Disabled),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], json!("glm-5.1"), "the requested id is sent");
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("low"));
    }

    /// Images go out as full `data:` URLs, the form Z.AI documents; bare
    /// base64 was verified equivalent on GLM-5.3-Flash, GLM-4.6V and GLM-4.5V.
    #[tokio::test]
    async fn images_are_sent_as_data_urls() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(chat_response()))
            .mount(&server)
            .await;
        let provider = provider_at("glm-5.3-flash", &server.uri());
        let png = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let mut message = Message::user("What colour is this?");
        message.content.push(crate::message::ContentPart::ImageUrl {
            image_url: crate::message::ImageUrl {
                url: png.to_string(),
                id: None,
            },
        });
        provider
            .generate(
                None,
                &[],
                &[message],
                GenerateOptions {
                    stream: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let parts = body["messages"][0]["content"].as_array().unwrap();
        let image = parts
            .iter()
            .find(|part| part["type"] == json!("image_url"))
            .expect("the image is sent");
        assert_eq!(image["image_url"]["url"], json!(png));
    }

    /// A model the plan does not include is refused with `1311`; the error
    /// says which models to use instead.
    #[tokio::test]
    async fn a_refused_model_names_the_models_to_use() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "error": {
                    "code": "1311",
                    "message": "Your current subscription plan does not yet include access to GLM-5V-Turbo"
                }
            })))
            .mount(&server)
            .await;
        let provider = provider_at("glm-5v-turbo", &server.uri()).with_coding_plan(true);
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
        let Err(LlmError::ApiBusinessError { code, message }) = result else {
            panic!("a plan refusal is a business error");
        };
        assert_eq!(code, "1311");
        assert!(
            message.contains("does not include glm-5v-turbo"),
            "{message}"
        );
        assert!(
            message.contains("use glm-5.3 or glm-5.3-flash"),
            "{message}"
        );
        assert!(
            message.contains("GLM-5V-Turbo"),
            "Z.AI's own words are kept: {message}"
        );
    }

    /// An empty balance on the pay-per-token endpoint says what it means for
    /// a Coding Plan key, the usual cause: use the plan's endpoint.
    #[tokio::test]
    async fn an_empty_balance_names_the_coding_plan_endpoint() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "error": {
                    "code": "1113",
                    "message": "Insufficient balance or no resource package. Please recharge."
                }
            })))
            .mount(&server)
            .await;
        for coding_plan in [false, true] {
            let provider = provider_at("glm-5.3", &server.uri()).with_coding_plan(coding_plan);
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
            let Err(LlmError::ApiBusinessError { code, message }) = result else {
                panic!("an empty balance is a business error");
            };
            assert_eq!(code, "1113");
            assert!(message.contains("Insufficient balance"), "{message}");
            assert_eq!(
                message.contains("use --billing subscription"),
                !coding_plan,
                "{message}"
            );
        }
    }

    // ---- tool_stream ------------------------------------------------------

    /// z.ai buffers the whole `arguments` string unless `tool_stream` is set, so
    /// a long tool payload arrives as one stall at the end of the turn.
    #[test]
    fn tool_stream_is_requested_when_streaming_with_tools() {
        let options = GenerateOptions {
            stream: true,
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&options, "glm-5.3", None, true);
        assert_eq!(extra["tool_stream"], json!(true));
    }

    /// It is meaningless without a stream to fill or a tool to call, so it is
    /// not sent there.
    #[test]
    fn tool_stream_is_omitted_without_a_stream_or_tools() {
        let streaming_no_tools = GenerateOptions {
            stream: true,
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&streaming_no_tools, "glm-5.3", None, false);
        assert!(extra.get("tool_stream").is_none());

        let tools_no_stream = GenerateOptions {
            stream: false,
            ..Default::default()
        };
        let extra = GlmChatProvider::build_extra_body(&tools_no_stream, "glm-5.3", None, true);
        assert!(extra.get("tool_stream").is_none());
    }

    /// z.ai documents `tool_stream` for the glm-4.6, glm-4.7 and glm-5 lines.
    /// Anything older must not be sent a parameter its schema does not declare.
    #[test]
    fn tool_stream_is_gated_to_glm_4_6_and_newer() {
        for model in [
            "glm-4.6",
            "glm-4.7",
            "glm-5.1",
            "glm-5.2",
            "glm-5.3",
            "glm-5-turbo",
            "glm-5v-turbo",
        ] {
            assert!(
                GlmChatProvider::supports_tool_stream(model),
                "{model} must accept tool_stream"
            );
        }
        for model in ["glm-4.5", "glm-4.5-air", "glm-4.0", "", "llama-3.1"] {
            assert!(
                !GlmChatProvider::supports_tool_stream(model),
                "{model} must not be sent tool_stream"
            );
        }
    }

    // ---- older models -----------------------------------------------------

    /// **The GLM-5.3 work must not cost anyone an older model.** Every model
    /// z.ai still serves keeps the request body it had: thinking switchable, its
    /// own effort vocabulary, and no parameter its generation does not declare.
    #[test]
    fn pre_5_3_models_keep_their_wire_shape() {
        for model in [
            "glm-5.2",
            "glm-5.1",
            "glm-5-turbo",
            "glm-5v-turbo",
            "glm-4.7",
            "glm-4.7-flash",
            "glm-4.6",
            "glm-4.5",
            "glm-4.5-air",
            "glm-4.5-flash",
        ] {
            assert!(
                !GlmChatProvider::thinking_is_mandatory(model),
                "{model} must keep its thinking off-switch"
            );
            assert_eq!(
                GlmChatProvider::fold_reasoning_effort(model, "medium"),
                "medium",
                "{model} must keep its own effort vocabulary"
            );
            let off = GenerateOptions {
                thinking: Some(ThinkingConfig::Disabled),
                ..Default::default()
            };
            let extra = GlmChatProvider::build_extra_body(&off, model, None, false);
            assert_eq!(
                extra["thinking"],
                json!({"type": "disabled"}),
                "{model} must still be able to switch thinking off"
            );
            assert!(
                extra.get("reasoning_effort").is_none(),
                "{model} must not carry effort on a thinking-off request"
            );
        }
    }

    /// The 4.5 series predates `tool_stream`; an undeclared parameter is a 400.
    #[test]
    fn the_4_5_series_and_older_never_receive_tool_stream() {
        let streaming = GenerateOptions {
            stream: true,
            ..Default::default()
        };
        for model in [
            "glm-4.5",
            "glm-4.5-air",
            "glm-4.5-x",
            "glm-4.5-airx",
            "glm-4.5-flash",
            "glm-4-32b-0414-128k",
        ] {
            let extra = GlmChatProvider::build_extra_body(&streaming, model, None, true);
            assert!(
                extra.get("tool_stream").is_none(),
                "{model} predates tool_stream and must not be sent it"
            );
        }
    }

    /// Context windows drive compaction, so a wrong one makes a model compact
    /// far more often than it needs to. Every figure here is z.ai's published one.
    #[test]
    fn every_catalogued_model_resolves_to_its_published_window() {
        for (model, window) in [
            ("glm-5.3", 1_048_576u64),
            ("glm-5.3-flash", 1_048_576),
            ("glm-5.3-flashx", 1_048_576),
            ("glm-5.2", 1_048_576),
            ("glm-5.1", 200_000),
            ("glm-5", 200_000),
            ("glm-5-turbo", 200_000),
            ("glm-5v-turbo", 200_000),
            ("glm-4.6v", 128_000),
            ("glm-4.6v-flash", 128_000),
            ("glm-4.6v-flashx", 128_000),
            ("glm-4.5v", 64_000),
            ("glm-4.7", 200_000),
            ("glm-4.7-flash", 200_000),
            ("glm-4.7-flashx", 200_000),
            ("glm-4.6", 200_000),
            ("glm-4.5", 128_000),
            ("glm-4.5-air", 128_000),
            ("glm-4.5-x", 128_000),
            ("glm-4.5-airx", 128_000),
            ("glm-4.5-flash", 128_000),
            ("glm-4-32b-0414-128k", 128_000),
        ] {
            assert_eq!(
                GlmChatProvider::context_window_for(model),
                window,
                "{model} must resolve to its published window"
            );
        }
    }

    /// Sampling follows the documented inclusive ranges; a deliberate `1.0` —
    /// GLM-5.3's own default — must survive rather than becoming `0.99`.
    #[test]
    fn documented_sampling_bounds_pass_through_unaltered() {
        let mut options = GenerateOptions {
            temperature: Some(1.0),
            top_p: Some(1.0),
            ..Default::default()
        };
        GlmChatProvider::sanitize_options(&mut options);
        assert_eq!(options.temperature, Some(1.0));
        assert_eq!(options.top_p, Some(1.0));
        assert_eq!(options.do_sample, None, "1.0 is sampling, not greedy");

        let mut floor = GenerateOptions {
            top_p: Some(0.001),
            ..Default::default()
        };
        GlmChatProvider::sanitize_options(&mut floor);
        assert_eq!(floor.top_p, Some(0.01));
    }

    #[test]
    fn test_media_policy_follows_the_catalogue() {
        let text = GlmChatProvider::media_policy_for("glm-5.3");
        assert!(!text.takes_images());
        assert!(!text.takes_video());
        let vision = GlmChatProvider::media_policy_for("glm-5v-turbo");
        assert!(vision.accepts_image("image/png"));
        assert!(!vision.accepts_image("image/bmp"));
        assert!(!vision.takes_video());
        let older = GlmChatProvider::media_policy_for("glm-4.6v");
        assert!(older.accepts_image("image/jpeg"));
        assert!(
            !older.takes_video(),
            "inline video is verified on GLM-5.3-Flash only"
        );
    }

    #[tokio::test]
    async fn test_generate_uses_max_tokens_and_do_sample() {
        use std::time::Duration;

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = json!({
            "id": "chatcmpl-test",
            "choices": [{
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let provider = GlmChatProvider::new(
            "glm-5.1",
            Some("sk-test".to_string()),
            server.uri(),
            HttpTimeouts::idle(Duration::from_secs(5)),
        )
        .unwrap();

        let _ = provider
            .generate(
                None,
                &[],
                &[],
                GenerateOptions {
                    stream: false,
                    max_tokens: Some(123),
                    temperature: Some(0.0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["max_tokens"], json!(123));
        assert!(body.get("max_completion_tokens").is_none());
        assert_eq!(body["do_sample"], json!(false));
    }

    #[tokio::test]
    async fn test_generate_clamps_max_tokens_to_api_limit() {
        use std::time::Duration;

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = json!({
            "id": "chatcmpl-test",
            "choices": [{
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let provider = GlmChatProvider::new(
            "glm-5.2",
            Some("sk-test".to_string()),
            server.uri(),
            HttpTimeouts::idle(Duration::from_secs(5)),
        )
        .unwrap();

        let _ = provider
            .generate(
                None,
                &[],
                &[],
                GenerateOptions {
                    stream: false,
                    max_tokens: Some(1_000_000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["max_tokens"], json!(131_072));
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[tokio::test]
    async fn test_generate_emits_reasoning_effort_and_thinking() {
        use std::time::Duration;

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let response = json!({
            "id": "chatcmpl-test",
            "choices": [{
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        });
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .mount(&server)
            .await;

        let provider = GlmChatProvider::new(
            "glm-5.2",
            Some("sk-test".to_string()),
            server.uri(),
            HttpTimeouts::idle(Duration::from_secs(5)),
        )
        .unwrap();

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
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["reasoning_effort"], json!("max"));
    }
}
