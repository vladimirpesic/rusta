use std::collections::HashMap;

use serde_json::{Value, json};

use crate::{
    errors::LlmError,
    media::{self, KIMI_IMAGE_MIMES, MediaPolicy, VideoSupport},
    message::{ContentPart, Message},
    provider::{ChatProvider, GenerateOptions, StreamedMessage, ThinkingConfig},
    tool::Tool,
};

use super::{
    kimi_catalog::{KimiModelInfo, ThinkingSupport, is_k_series, is_platform_k3_series},
    kimi_files::{self, KimiFiles, Purpose},
    kimi_schema::normalize_kimi_tool_schema,
    openai_compat::{HttpTimeouts, OpenAiCompatProvider},
};

/// The models the Kimi Code endpoints serve, in their catalogue's order (its
/// first is its default), as `GET /models` lists them.
const KIMI_CODING_MODELS: &[&str] = &[
    "kimi-for-coding",
    "kimi-for-coding-highspeed",
    "k3",
    "k3-256k",
    "k3.1-pro",
    "k3.1-pro-highspeed",
];

/// The models the pay-per-token platform serves, newest first, as its model
/// list orders them.
const PLATFORM_MODELS: &[&str] = &[
    "kimi-k3.1-pro",
    "kimi-k3.1-pro-highspeed",
    "kimi-k3",
    "kimi-k2.7-code",
    "kimi-k2.7-code-highspeed",
    "kimi-k2.6",
];

/// Hosts of the Kimi Code endpoints, where media goes through the Files API.
/// The pay-per-token platform (`api.moonshot.ai`) is not one: uploads there
/// are unverified, so its media stays inline.
const KIMI_CODING_HOSTS: &[&str] = &["api.kimi.com", "api.kimi.ai"];

/// The only `thinking.keep` value Rusta emits: retain every prior reasoning
/// block. Moonshot forwards the field verbatim, so this is a string, not a bool.
const THINKING_KEEP_ALL: &str = "all";

/// How a requested reasoning effort maps onto Kimi (the models page's
/// "Effort Level Mapping").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortChoice {
    /// Turn thinking off.
    Off,
    /// One of `low`, `high`, `max`.
    Level(&'static str),
}

/// Native Moonshot (Kimi) provider.
pub struct KimiChatProvider {
    inner: OpenAiCompatProvider,
    /// Kimi's Files API, on the Kimi Code endpoints: images and video are
    /// uploaded once and sent by reference.
    files: Option<KimiFiles>,
    /// What the configured model can do: the live catalogue's entry when one
    /// was supplied, the built-in table otherwise.
    info: KimiModelInfo,
    /// The live catalogue, when supplied: what `known_models` lists.
    catalog: Option<Vec<KimiModelInfo>>,
    /// The other Kimi Code host, tried when this one refuses the key.
    alternate: Option<Box<KimiChatProvider>>,
    /// The alternate host accepted the key; later requests go straight there.
    on_alternate: std::sync::atomic::AtomicBool,
}

impl KimiChatProvider {
    pub fn new(
        model: impl Into<String>,
        api_key: Option<String>,
        base_url: impl Into<String>,
        timeouts: HttpTimeouts,
    ) -> Result<Self, LlmError> {
        let model = model.into();
        let info = KimiModelInfo::builtin(&model);
        let inner = OpenAiCompatProvider::new("kimi", model, base_url, api_key, timeouts)?
            // Moonshot rejects properties its request schema does not
            // declare, and `reasoning_details` is not one of them.
            .with_reasoning_details_echo(false);
        let uploads = is_kimi_coding_endpoint(inner.base_url());
        Ok(Self {
            inner,
            files: None,
            info,
            catalog: None,
            alternate: None,
            on_alternate: std::sync::atomic::AtomicBool::new(false),
        }
        .with_file_uploads(uploads))
    }

    /// Fall back to `base_url` — the other Kimi Code host — when this one
    /// refuses the key (401). A key from a kimi.ai account can be refused by
    /// api.kimi.com, and vice versa, with an error that reads like a bad key.
    /// Call last: the fallback copies the catalogue and upload cache configured
    /// so far.
    pub fn with_alternate_host(
        mut self,
        base_url: &str,
        timeouts: HttpTimeouts,
    ) -> Result<Self, LlmError> {
        let mut alternate = KimiChatProvider::new(
            self.inner.model_name(),
            self.inner.api_key().map(str::to_string),
            base_url,
            timeouts,
        )?;
        if let Some(models) = &self.catalog {
            alternate = alternate.with_catalog(models.clone());
        }
        if let Some(dir) = self.files.as_ref().and_then(KimiFiles::cache_dir) {
            alternate = alternate.with_media_cache_dir(dir.to_path_buf());
        }
        // Whatever this host sends, the other one sends too.
        let alternate = alternate.with_custom_headers(&self.inner.extra_headers());
        self.alternate = Some(Box::new(alternate));
        Ok(self)
    }

    /// Take the configured model's capabilities from the live catalogue
    /// (`GET /models`, see [`super::kimi_catalog`]) instead of the built-in
    /// table, and list the catalogue's models as the known ones. A model the
    /// catalogue does not list keeps the built-in entry.
    #[must_use]
    pub fn with_catalog(mut self, models: Vec<KimiModelInfo>) -> Self {
        if let Some(entry) = models
            .iter()
            .find(|entry| entry.id.eq_ignore_ascii_case(self.inner.model_name()))
        {
            self.info = entry.clone();
        }
        self.catalog = Some(models);
        self.refresh_inner()
    }

    /// Declare the model's media support to the wire layer. It would
    /// otherwise guess from an OpenAI-shaped name and answer `false` for every
    /// Kimi model, silently degrading images on the ones that see.
    fn refresh_inner(mut self) -> Self {
        let policy = Self::media_policy_for(&self.info, self.files.is_some());
        self.inner = self
            .inner
            .with_vision(self.info.image_in)
            .with_media_policy(policy)
            .with_message_tools(self.info.dynamic_tools);
        self
    }

    /// `history` with the tool definitions its messages carry normalized for
    /// Kimi, as the top-level ones are; `None` when no message carries any.
    fn normalize_message_tools(history: &[Message]) -> Result<Option<Vec<Message>>, LlmError> {
        if history.iter().all(|m| m.tools.is_empty()) {
            return Ok(None);
        }
        history
            .iter()
            .map(|message| {
                if message.tools.is_empty() {
                    return Ok(message.clone());
                }
                let mut message = message.clone();
                message.tools = Self::normalize_tools(&message.tools)?;
                Ok(message)
            })
            .collect::<Result<Vec<_>, LlmError>>()
            .map(Some)
    }

    /// Send images and video through Kimi's Files API (`true`), or inline.
    /// On by default on the Kimi Code endpoints; needs an API key.
    #[must_use]
    pub fn with_file_uploads(mut self, enabled: bool) -> Self {
        self.files = match (enabled, self.inner.api_key()) {
            (true, Some(key)) => Some(KimiFiles::new(
                self.inner.http_client().clone(),
                self.inner.base_url(),
                key,
                self.inner.extra_headers(),
            )),
            _ => None,
        };
        self.refresh_inner()
    }

    /// Send `headers` with every request — chat, uploads, and the alternate
    /// host's — below the provider's own ([`crate::custom_headers`]).
    #[must_use]
    pub fn with_custom_headers(mut self, headers: &reqwest::header::HeaderMap) -> Self {
        self.inner = self.inner.with_custom_headers(headers);
        self.files = self.files.map(|files| files.with_custom_headers(headers));
        self.alternate = self
            .alternate
            .map(|alternate| Box::new(alternate.with_custom_headers(headers)));
        self
    }

    /// Remember uploads on disk under `dir`, so a resumed session reuses them.
    #[must_use]
    pub fn with_media_cache_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.files = self.files.map(|files| files.with_cache_dir(dir));
        self
    }

    /// Whether `model` takes video: every Kimi model but the 256K variant
    /// (`k3-256k`) does, per the Kimi Code catalogue and the platform's
    /// vision guide.
    pub fn supports_video_for(model: &str) -> bool {
        Self::supports_vision_for(model) && !model.to_lowercase().ends_with("-256k")
    }

    /// Which media a model with `info` takes: Kimi's image formats on vision
    /// models, and video where the model takes it and `uploads` can carry it.
    pub fn media_policy_for(info: &KimiModelInfo, uploads: bool) -> MediaPolicy {
        if !info.image_in {
            return MediaPolicy {
                image_mimes: &[],
                video: VideoSupport::Unsupported {
                    hint: "the current Kimi model does not take image or video input",
                },
                max_image_edge_px: None,
            };
        }
        let video = if !info.video_in {
            VideoSupport::Unsupported {
                hint: "the current Kimi model does not take video input",
            }
        } else if uploads {
            VideoSupport::Upload
        } else {
            VideoSupport::Unsupported {
                hint: "Kimi takes video through its Files API, which Rusta uses on the Kimi \
                       Code endpoints only",
            }
        };
        MediaPolicy {
            image_mimes: KIMI_IMAGE_MIMES,
            video,
            max_image_edge_px: None,
        }
    }

    /// `history` with its inline images and video uploaded and referenced by
    /// id, and the ids that came from earlier uploads (a stale one gets the
    /// request a 404). An image whose upload fails stays inline; a video that
    /// fails is replaced with a note. An authentication failure is returned:
    /// the chat request would only hit it again.
    async fn upload_media(
        &self,
        history: &[Message],
        files: &KimiFiles,
    ) -> Result<(Vec<Message>, Vec<String>), LlmError> {
        let policy = Self::media_policy_for(&self.info, true);
        let mut messages = history.to_vec();
        let mut remembered = Vec::new();
        for message in &mut messages {
            for part in &mut message.content {
                let (url, purpose) = match part {
                    ContentPart::ImageUrl { image_url } => (&image_url.url, Purpose::Image),
                    ContentPart::VideoUrl { video_url } => (&video_url.url, Purpose::Video),
                    _ => continue,
                };
                let Some((mime, data)) = media::parse_data_url(url) else {
                    continue;
                };
                let mime = media::normalize_mime(mime);
                let wanted = match purpose {
                    Purpose::Image => policy.accepts_image(&mime),
                    Purpose::Video => policy.video == VideoSupport::Upload,
                };
                if !wanted {
                    // The request's media normalization says why.
                    continue;
                }
                let replacement = match files.file_id(data, &mime, purpose).await {
                    Ok(file) => {
                        if file.remembered {
                            remembered.push(file.id.clone());
                        }
                        let url = format!("ms://{}", file.id);
                        match purpose {
                            Purpose::Image => ContentPart::ImageUrl {
                                image_url: crate::message::ImageUrl {
                                    url,
                                    id: Some(file.id),
                                },
                            },
                            Purpose::Video => ContentPart::VideoUrl {
                                video_url: crate::message::UrlRef {
                                    url,
                                    id: Some(file.id),
                                },
                            },
                        }
                    }
                    Err(e) if kimi_files::is_auth_error(&e) => return Err(e),
                    Err(e) if purpose == Purpose::Image => {
                        tracing::warn!("image upload to Kimi failed ({e}); sending it inline");
                        continue;
                    }
                    Err(e) => ContentPart::text(format!(
                        "[video omitted: uploading it to Kimi's Files API failed: {e}]"
                    )),
                };
                *part = replacement;
            }
        }
        Ok((messages, remembered))
    }

    fn normalize_tools(tools: &[Tool]) -> Result<Vec<Tool>, LlmError> {
        tools
            .iter()
            .map(|t| {
                if t.is_builtin_function() {
                    return Ok(t.clone());
                }
                let normalized = normalize_kimi_tool_schema(&t.parameters)?;
                Ok(Tool {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: normalized,
                })
            })
            .collect()
    }

    /// Context-window sizes for every model Kimi serves. Moonshot's retired
    /// models (`moonshot-v1-*`, `kimi-k2.5` and the dash-versioned `kimi-k2-*`
    /// ids) answer 404, so they are not listed.
    pub fn model_metadata() -> &'static HashMap<&'static str, u64> {
        use std::sync::LazyLock;
        static METADATA: LazyLock<HashMap<&str, u64>> = LazyLock::new(|| {
            [
                // The platform: every K3-series model has 1M, every K2 model
                // 262k.
                ("kimi-k3.1-pro", 1_048_576u64),
                ("kimi-k3.1-pro-highspeed", 1_048_576),
                ("kimi-k3", 1_048_576),
                ("kimi-k2.7-code", 262_144),
                ("kimi-k2.7-code-highspeed", 262_144),
                ("kimi-k2.6", 262_144),
                // The Kimi Code catalogue, as `GET /models` reports it:
                // `kimi-for-coding` is K2.8 Preview at 1M, the highspeed alias
                // is K2.7 at 262k, K3 comes in a 1M and a 262k variant, and
                // both K3.1 Pro models have 1M.
                ("kimi-for-coding", 1_048_576),
                ("kimi-for-coding-highspeed", 262_144),
                ("k3", 1_048_576),
                ("k3-256k", 262_144),
                ("k3.1-pro", 1_048_576),
                ("k3.1-pro-highspeed", 1_048_576),
            ]
            .into_iter()
            .collect()
        });
        &METADATA
    }

    /// Map a requested reasoning effort onto Kimi's vocabulary, per the
    /// models page's "Effort Level Mapping": `ultra`/`max`/`xhigh` → `max`,
    /// `high`/`medium` → `high`, `low`/`minimum`/`minimal`/`light` → `low`,
    /// `off`/`none`/`disabled` → thinking off. Anything else is `None`, and
    /// the field is left out so the model's own default applies.
    pub fn map_reasoning_effort(effort: &str) -> Option<EffortChoice> {
        match effort.trim().to_lowercase().as_str() {
            "ultra" | "max" | "xhigh" => Some(EffortChoice::Level("max")),
            "high" | "medium" => Some(EffortChoice::Level("high")),
            "low" | "minimum" | "minimal" | "light" => Some(EffortChoice::Level("low")),
            "off" | "none" | "disabled" => Some(EffortChoice::Off),
            _ => None,
        }
    }

    /// `options` without a temperature or top-p. Every model Kimi serves
    /// fixes them — temperature 1.0 (0.6 for `kimi-k2.6` without thinking),
    /// top-p 0.95 — and answers any other value with a 400 ("invalid
    /// temperature: only 1 is allowed for this model"), so a configured value
    /// is dropped, with a warning once.
    fn without_fixed_sampling(mut options: GenerateOptions) -> GenerateOptions {
        if options.temperature.is_some() || options.top_p.is_some() {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    "Kimi fixes temperature and top_p for every model it serves and rejects \
                     other values; the configured ones are not sent"
                );
            });
            options.temperature = None;
            options.top_p = None;
        }
        options
    }

    pub fn context_window_for(model: &str) -> u64 {
        // Exact entry, then the nearest lower catalogued minor — so a
        // `kimi-k2.8` this table does not list yet inherits
        // `kimi-k2.6`'s 262k window rather than dropping to the 128k default.
        if let Some(window) =
            crate::model_version::lookup_with_minor_fallback(model, Self::model_metadata())
        {
            return window;
        }
        let lower = model.to_lowercase();
        if lower.ends_with("-1m") {
            return 1_048_576;
        }
        if lower.ends_with("-256k") {
            return 262_144;
        }
        if lower.ends_with("-128k") {
            return 131_072;
        }
        if lower.ends_with("-32k") {
            return 32_768;
        }
        if lower.ends_with("-8k") {
            return 8_192;
        }
        128_000
    }

    /// The models an endpoint serves when its catalogue is not at hand: the
    /// Kimi Code endpoints' (`kimi_coding`), or the platform's. Built from the
    /// built-in table, never hand-written: a hand-written vision flag once
    /// drifted from `supports_vision_for` and silently stripped every image
    /// sent to `kimi-for-coding`.
    fn builtin_models(kimi_coding: bool) -> Vec<KimiModelInfo> {
        let ids = if kimi_coding {
            KIMI_CODING_MODELS
        } else {
            PLATFORM_MODELS
        };
        ids.iter().map(|id| KimiModelInfo::builtin(id)).collect()
    }

    /// Whether `model` accepts `image_url` content parts. Every model Kimi
    /// serves does — the platform's `kimi-k2.6`, `kimi-k2.7-code` and K3
    /// series, and every Kimi Code model — so any Kimi id is taken to,
    /// including one Rusta does not know yet.
    ///
    /// The two failure modes are not symmetric: a false negative makes
    /// `downgrade_unsupported_media` silently replace every image with a text
    /// marker, while a false positive surfaces as a provider error. Prefer the
    /// loud one; the live catalogue names a model that takes no images.
    pub fn supports_vision_for(model: &str) -> bool {
        let lower = model.to_lowercase();
        lower.starts_with("kimi") || is_k_series(&lower)
    }

    /// Whether `model` is known to take `thinking.keep`, so Preserved
    /// Thinking goes on without being asked for.
    ///
    /// The platform's request schemas carry the field from `kimi-k2.6` on.
    /// The Kimi Code ids carry no version to compare, and `kimi-for-coding`
    /// is the default model on that plan: leaving it out would quietly ship
    /// Preserved Thinking off for most users. So they were probed instead of
    /// guessed — the Coding endpoint answers 200 to `thinking.keep: "all"`
    /// for both `kimi-for-coding` aliases and for `k3.1-pro`, and returns
    /// `reasoning_content` — and the `k` series always reasons and keeps it.
    ///
    /// Any other id gets the field only when asked: Moonshot rejects a
    /// property a model's schema does not declare.
    fn takes_thinking_keep(model: &str) -> bool {
        let lower = model.to_lowercase();
        if lower.starts_with("kimi-for-coding") || is_k_series(&lower) {
            return true;
        }
        crate::model_version::parse(&lower).is_some_and(|version| {
            version.prefix == "kimi-k"
                && version
                    .major
                    .parse::<u32>()
                    .is_ok_and(|major| major > 2 || (major == 2 && version.minor >= 6))
        })
    }

    /// The Kimi-specific request fields for a model with `info`.
    fn build_extra_body(options: &GenerateOptions, info: &KimiModelInfo) -> Value {
        let mut extra = json!({});
        if let Some(key) = &options.prompt_cache_key {
            extra["prompt_cache_key"] = json!(key);
        }
        if info.thinking == ThinkingSupport::No {
            // A model the catalogue lists as never thinking takes no
            // `thinking` object at all.
            return extra;
        }

        let effort = options
            .reasoning_effort
            .as_deref()
            .and_then(Self::map_reasoning_effort);
        // Off — asked for directly, or as effort `none`. Kimi Code serves
        // its other models with K2.8 Preview, without thinking, so there an
        // off request is honoured rather than silently ignored — and never
        // carries an effort.
        let off = matches!(options.thinking, Some(ThinkingConfig::Disabled))
            || effort == Some(EffortChoice::Off);
        // A model that cannot turn thinking off gets the floor of its ladder
        // — Kimi's own advice for K3.1 Pro: "set reasoning_effort to low" —
        // rather than a 400, and not its configured effort: otherwise
        // "thinking = false" would silently run `max`.
        let forced_on = off && cannot_turn_thinking_off(&info.id);
        if off && !forced_on {
            extra["thinking"] = json!({"type": "disabled"});
            return extra;
        }

        // Only for a model with an effort ladder, and on the nearest level it
        // accepts.
        let requested = match effort {
            _ if forced_on => Some("low"),
            Some(EffortChoice::Level(level)) => Some(level),
            _ => None,
        };
        let level = requested.and_then(|level| {
            info.efforts
                .as_ref()
                .and_then(|ladder| ladder.nearest(level))
        });

        // The platform's K3 series takes its effort as a top-level
        // `reasoning_effort`: `thinking` is a K2.x parameter there. It always
        // thinks, and always preserves its reasoning. It is also the series
        // that bills cache writes by lifetime, so it is told the one asked
        // for; left out, the platform writes for five minutes.
        if is_platform_k3_series(&info.id) {
            if let Some(level) = level {
                extra["reasoning_effort"] = json!(level);
            }
            if let Some(ttl) = options.prompt_cache_ttl {
                extra["prompt_cache_options"] = json!({"mode": "implicit", "ttl": ttl.as_str()});
            }
            return extra;
        }

        // Everywhere else the effort goes inside the thinking object, where
        // Kimi Code validates it: `low`/`high`/`max` are accepted and
        // anything else is a 400, while a bogus top-level `reasoning_effort`
        // is ignored.
        let thinking_enabled = forced_on
            || matches!(
                options.thinking,
                Some(ThinkingConfig::Enabled) | Some(ThinkingConfig::EnabledExplicit)
            )
            || options.thinking_keep == Some(true)
            || level.is_some();

        if thinking_enabled {
            let mut thinking = json!({"type": "enabled"});
            if let Some(level) = level {
                thinking["effort"] = json!(level);
            }
            // Preserved Thinking is on by default wherever Moonshot supports
            // it, so a turn's `reasoning_content` survives into the next one
            // instead of being dropped.
            //
            // `keep` is a *string* naming what to retain, not a boolean: its
            // enum is `["all", null]`, and `"all"` prunes nothing. Disabling it
            // means omitting the field — Moonshot has no `keep: false`.
            let send_keep = if Self::takes_thinking_keep(&info.id) {
                options.thinking_keep.unwrap_or(true)
            } else {
                options.thinking_keep == Some(true)
            };
            if send_keep {
                thinking["keep"] = json!(THINKING_KEEP_ALL);
            }
            extra["thinking"] = thinking;
        }
        extra
    }
}

#[async_trait::async_trait]
impl ChatProvider for KimiChatProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn context_window(&self) -> u64 {
        self.info.context_length
    }

    fn supports_vision(&self) -> bool {
        self.info.image_in
    }

    fn media_policy(&self) -> MediaPolicy {
        Self::media_policy_for(&self.info, self.files.is_some())
    }

    fn supports_dynamic_tools(&self) -> bool {
        self.info.dynamic_tools
    }

    /// Kimi keeps `window − input`: the model is entitled to the whole rest
    /// of the context window when no cap is configured. An unknown window
    /// sends nothing.
    fn completion_tokens_when_unset(&self, input_tokens: u64, context_window: u64) -> Option<u32> {
        if context_window == 0 {
            return None;
        }
        let room = context_window.saturating_sub(input_tokens).max(1);
        Some(u32::try_from(room).unwrap_or(u32::MAX))
    }

    fn known_models(&self) -> Vec<crate::provider::ModelInfo> {
        let builtin;
        let models = match &self.catalog {
            Some(models) => models,
            None => {
                builtin = Self::builtin_models(is_kimi_coding_endpoint(self.inner.base_url()));
                &builtin
            }
        };
        models
            .iter()
            .map(|model| crate::provider::ModelInfo {
                id: model.id.clone(),
                display_name: model.display_name.clone(),
                context_window: model.context_length,
                supports_vision: model.image_in,
            })
            .collect()
    }

    fn builtin_tools(&self) -> Vec<Tool> {
        vec![Tool::new(
            "$web_search",
            "Search the web: the Kimi Coding Plan's search service, or a configured search URL template (RUSTA_SEARCH_URL / search_url), which takes precedence.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "timeout": { "type": "integer", "description": "Timeout in seconds (default 30)" }
                },
                "required": ["query"]
            }),
        )]
    }

    async fn generate(
        &self,
        system_prompt: Option<&str>,
        tools: &[Tool],
        history: &[Message],
        options: GenerateOptions,
    ) -> Result<StreamedMessage, LlmError> {
        use std::sync::atomic::Ordering;
        let Some(alternate) = &self.alternate else {
            return self
                .generate_here(system_prompt, tools, history, options)
                .await;
        };
        if self.on_alternate.load(Ordering::Relaxed) {
            return alternate
                .generate_here(system_prompt, tools, history, options)
                .await;
        }
        match self
            .generate_here(system_prompt, tools, history, options.clone())
            .await
        {
            Err(refused) if is_refused_key(&refused) => {
                match alternate
                    .generate_here(system_prompt, tools, history, options)
                    .await
                {
                    Ok(streamed) => {
                        self.on_alternate.store(true, Ordering::Relaxed);
                        tracing::warn!(
                            "{} refused the API key but {} accepted it; this session uses the \
                             latter. Pin it with `base_url = \"{}\"` in the config, or run \
                             `rusta init`.",
                            self.inner.base_url(),
                            alternate.inner.base_url(),
                            alternate.inner.base_url()
                        );
                        Ok(streamed)
                    }
                    // Both hosts refused the key: this host's answer says so.
                    Err(e) if is_refused_key(&e) => Err(refused),
                    // The other host took the key and turned the request down
                    // for another reason, such as a plan limit: that is the
                    // answer that matters.
                    Err(e) => Err(e),
                }
            }
            other => other,
        }
    }
}

impl KimiChatProvider {
    /// One request to this provider's own host.
    async fn generate_here(
        &self,
        system_prompt: Option<&str>,
        tools: &[Tool],
        history: &[Message],
        options: GenerateOptions,
    ) -> Result<StreamedMessage, LlmError> {
        let options = Self::without_fixed_sampling(options);
        let normalized_tools = Self::normalize_tools(tools)?;
        let normalized_history = Self::normalize_message_tools(history)?;
        let history = normalized_history.as_deref().unwrap_or(history);
        let extra_body = Self::build_extra_body(&options, &self.info);
        let has_inline_media = history.iter().any(|m| {
            m.content
                .iter()
                .any(|p| media::media_url(p).is_some_and(media::is_data_url))
        });
        let Some(files) = self.files.as_ref().filter(|_| has_inline_media) else {
            return self
                .inner
                .generate(
                    system_prompt,
                    &normalized_tools,
                    history,
                    options,
                    extra_body,
                )
                .await
                .map_err(retry_membership_verification);
        };

        let (prepared, remembered) = self.upload_media(history, files).await?;
        let result = self
            .inner
            .generate(
                system_prompt,
                &normalized_tools,
                &prepared,
                options.clone(),
                extra_body.clone(),
            )
            .await;
        match result {
            // A remembered upload Kimi no longer has: upload afresh, once.
            Err(e) if kimi_files::is_missing_file(&e) && !remembered.is_empty() => {
                tracing::warn!(
                    "Kimi no longer has {} remembered upload(s); uploading them again",
                    remembered.len()
                );
                files.forget(&remembered);
                let (prepared, _) = self.upload_media(history, files).await?;
                self.inner
                    .generate(
                        system_prompt,
                        &normalized_tools,
                        &prepared,
                        options,
                        extra_body,
                    )
                    .await
            }
            other => other,
        }
        .map_err(retry_membership_verification)
    }
}

/// Whether the endpoint refused the key itself (HTTP 401).
/// Whether `error` is a Coding host refusing the key itself, which is what a
/// key gets from the other host. Kimi also answers `401` for a valid key whose
/// plan does not cover the request (no K3 access, K3 past 256K, no HighSpeed,
/// an unknown model id); another host cannot fix that, and its own refusal
/// would hide the reason.
fn is_refused_key(error: &LlmError) -> bool {
    matches!(
        error,
        LlmError::ApiStatus { status, message, .. }
            if *status == reqwest::StatusCode::UNAUTHORIZED && !is_plan_limit(message)
    )
}

/// Kimi's `401` permission errors, as its error reference words them.
fn is_plan_limit(message: &str) -> bool {
    let lower = message.to_lowercase();
    [
        "does not have access to",
        "supports only",
        "model id does not exist",
        "higher-tier",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
}

/// Whether `model` refuses to have its thinking turned off: Kimi Code's
/// K3.1 series and later (`k3.1-pro` answers 400 "does not support disabled
/// thinking mode"), the platform's K3 series, which has no off switch, and
/// `kimi-k2.7-code`, whose thinking "cannot be disabled". Kimi Code serves
/// `k3`, `k3-256k` and the `kimi-for-coding` aliases with K2.8 Preview,
/// without thinking, instead.
fn cannot_turn_thinking_off(model: &str) -> bool {
    let lower = model.to_lowercase();
    if is_platform_k3_series(&lower) || lower.starts_with("kimi-k2.7-code") {
        return true;
    }
    k_series_version(&lower).is_some_and(|version| version >= (3, 1))
}

/// The `(major, minor)` a Kimi Code `k` id names: `k3.1-pro` is `(3, 1)`,
/// `k3` and `k3-256k` are `(3, 0)`.
fn k_series_version(model: &str) -> Option<(u32, u32)> {
    if !is_k_series(model) {
        return None;
    }
    let number = |text: &str| -> Option<u32> {
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        text[..digits].parse().ok()
    };
    let rest = &model[1..];
    let major = number(rest)?;
    let minor = rest
        .split_once('.')
        .filter(|(before, _)| before.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|(_, after)| number(after))
        .unwrap_or(0);
    Some((major, minor))
}

/// Whether `base_url` is a Kimi Code endpoint (`…/coding/…` on a Kimi host).
fn is_kimi_coding_endpoint(base_url: &str) -> bool {
    url::Url::parse(base_url).is_ok_and(|url| {
        url.host_str()
            .is_some_and(|host| KIMI_CODING_HOSTS.contains(&host))
            && url.path().starts_with("/coding")
    })
}

/// Floor for the backoff before retrying a Kimi `402`.
const MEMBERSHIP_RETRY_FLOOR: std::time::Duration = std::time::Duration::from_secs(5);

/// Mark Kimi's `402 Unable to verify membership benefits` as retryable.
///
/// Kimi's error reference lists it as transient — the membership check is
/// briefly unavailable — and says to retry after a short wait. The flag is set
/// here, in the Kimi provider, rather than for every endpoint: elsewhere a
/// `402` means spent credits (OpenRouter), which no retry fixes.
fn retry_membership_verification(error: LlmError) -> LlmError {
    match error {
        LlmError::ApiStatus {
            status,
            message,
            retry_after,
            ..
        } if status == reqwest::StatusCode::PAYMENT_REQUIRED => LlmError::ApiStatus {
            status,
            message,
            retry_hint: true,
            retry_after: Some(retry_after.map_or(MEMBERSHIP_RETRY_FLOOR, |server| {
                server.max(MEMBERSHIP_RETRY_FLOOR)
            })),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No Kimi request is given a temperature of Rusta's choosing: each model
    /// runs at the one Kimi fixes for it.
    #[test]
    fn temperature_is_never_injected_for_kimi_models() {
        for model in ["kimi-k2.6", "kimi-k2.7-code", "kimi-for-coding", "k3"] {
            let options = GenerateOptions {
                thinking: Some(ThinkingConfig::Enabled),
                ..Default::default()
            };
            let extra =
                KimiChatProvider::build_extra_body(&options, &KimiModelInfo::builtin(model));
            assert!(
                extra.get("temperature").is_none(),
                "{model} must not be sent an injected temperature"
            );
        }
    }

    /// Models without an effort ladder never get an effort: not at the top
    /// level (Kimi ignores it there) and not in `thinking` (it would be
    /// rejected).
    #[test]
    fn reasoning_effort_is_never_sent_to_models_without_a_ladder() {
        for model in ["kimi-k2.6", "kimi-k2.7-code", "kimi-for-coding-highspeed"] {
            let options = GenerateOptions {
                thinking: Some(ThinkingConfig::Enabled),
                reasoning_effort: Some("high".to_string()),
                ..Default::default()
            };
            let extra =
                KimiChatProvider::build_extra_body(&options, &KimiModelInfo::builtin(model));
            assert!(
                extra.get("reasoning_effort").is_none(),
                "{model} must not be sent reasoning_effort"
            );
            assert!(extra["thinking"].get("effort").is_none(), "{model}");
        }
    }

    /// Kimi's effort vocabulary, per its models page, plus `minimal`.
    #[test]
    fn every_documented_effort_alias_maps() {
        for (input, expected) in [
            ("ultra", EffortChoice::Level("max")),
            ("max", EffortChoice::Level("max")),
            ("xhigh", EffortChoice::Level("max")),
            ("high", EffortChoice::Level("high")),
            ("medium", EffortChoice::Level("high")),
            ("low", EffortChoice::Level("low")),
            ("minimum", EffortChoice::Level("low")),
            ("minimal", EffortChoice::Level("low")),
            ("light", EffortChoice::Level("low")),
            (" None ", EffortChoice::Off),
            ("off", EffortChoice::Off),
            ("disabled", EffortChoice::Off),
        ] {
            assert_eq!(
                KimiChatProvider::map_reasoning_effort(input),
                Some(expected),
                "{input}"
            );
        }
        assert_eq!(KimiChatProvider::map_reasoning_effort("turbo"), None);
    }

    #[test]
    fn test_build_extra_body_includes_prompt_cache_key() {
        let options = GenerateOptions {
            prompt_cache_key: Some("session-42".to_string()),
            ..Default::default()
        };
        let extra = KimiChatProvider::build_extra_body(
            &options,
            &KimiModelInfo::builtin("kimi-for-coding"),
        );
        assert_eq!(extra["prompt_cache_key"], json!("session-42"));
    }

    /// Asking to keep thinking turns thinking on, with `keep`.
    #[test]
    fn test_build_extra_body_keeps_thinking_when_requested() {
        let options = GenerateOptions {
            thinking_keep: Some(true),
            ..Default::default()
        };
        let extra =
            KimiChatProvider::build_extra_body(&options, &KimiModelInfo::builtin("kimi-k2.6"));
        assert_eq!(extra["thinking"]["type"], json!("enabled"));
        assert_eq!(extra["thinking"]["keep"], json!("all"));
    }

    /// `thinking.keep` arrived with kimi-k2.6; the Kimi Code ids were probed.
    #[test]
    fn the_models_known_to_take_thinking_keep() {
        for (model, expected) in [
            ("kimi-k2.6", true),
            ("kimi-k2.7-code", true),
            ("Kimi-K2.7-Code", true),
            ("kimi-k2.8", true),
            ("kimi-k3.0", true),
            // Probed against the live Coding Plan endpoint, not inferred: both
            // aliases answer 200 to `keep: "all"`. They are the default model
            // on that plan, so leaving them out would disable Preserved
            // Thinking for most users.
            ("kimi-for-coding", true),
            ("kimi-for-coding-highspeed", true),
            ("k3", true),
            ("k3.1-pro", true),
            ("kimi-next", false),
            ("", false),
        ] {
            assert_eq!(
                KimiChatProvider::takes_thinking_keep(model),
                expected,
                "{model}"
            );
        }
    }

    /// Both Kimi defaults preserve thinking without being asked:
    /// `kimi-for-coding`, the Coding Plan's, is sent `thinking.keep`, and
    /// `kimi-k3.1-pro`, the platform's, always keeps its reasoning and is
    /// sent no `thinking` at all. A miss means the default silently runs
    /// without Preserved Thinking.
    #[test]
    fn both_kimi_defaults_preserve_thinking() {
        let thinking_on = GenerateOptions {
            thinking: Some(ThinkingConfig::Enabled),
            thinking_keep: None,
            ..Default::default()
        };
        assert!(KimiChatProvider::takes_thinking_keep("kimi-for-coding"));
        let extra = extra(thinking_on.clone(), "kimi-for-coding");
        assert_eq!(extra["thinking"]["keep"], THINKING_KEEP_ALL);
        let platform = KimiModelInfo::builtin("kimi-k3.1-pro");
        assert_eq!(platform.thinking, ThinkingSupport::Only);
        assert!(
            KimiChatProvider::build_extra_body(&thinking_on, &platform)
                .get("thinking")
                .is_none()
        );
    }

    /// Preserved Thinking rides along with Thinking, without being asked for,
    /// on every model whose schema declares `keep`.
    #[test]
    fn preserved_thinking_defaults_on_for_kimi_models_when_thinking_is_on() {
        for model in ["kimi-k2.6", "kimi-k2.7-code", "Kimi-K2.7-Code"] {
            let options = GenerateOptions {
                thinking: Some(ThinkingConfig::EnabledExplicit),
                thinking_keep: None,
                ..Default::default()
            };
            let extra =
                KimiChatProvider::build_extra_body(&options, &KimiModelInfo::builtin(model));
            assert_eq!(extra["thinking"]["type"], json!("enabled"), "{model}");
            assert_eq!(extra["thinking"]["keep"], json!("all"), "{model}");
        }
    }

    /// An alias carrying no version — and which we have not probed — could be
    /// backed by a schema without the field, and Moonshot rejects an undeclared
    /// property outright. So `keep` is omitted by default there, while an
    /// explicit request is still honoured.
    #[test]
    fn uncatalogued_alias_omits_keep_by_default_but_honours_an_explicit_ask() {
        let defaulted = GenerateOptions {
            thinking: Some(ThinkingConfig::EnabledExplicit),
            thinking_keep: None,
            ..Default::default()
        };
        let extra =
            KimiChatProvider::build_extra_body(&defaulted, &KimiModelInfo::builtin("kimi-next"));
        assert_eq!(extra["thinking"]["type"], json!("enabled"));
        assert!(extra["thinking"].get("keep").is_none());

        let asked = GenerateOptions {
            thinking_keep: Some(true),
            ..defaulted
        };
        let extra =
            KimiChatProvider::build_extra_body(&asked, &KimiModelInfo::builtin("kimi-next"));
        assert_eq!(extra["thinking"]["keep"], json!("all"));
    }

    /// Moonshot's `thinking.keep` names *what* to retain and is forwarded
    /// verbatim; `"all"` prunes nothing. A boolean `true` is not the contract.
    #[test]
    fn thinking_keep_is_the_string_all_not_a_boolean() {
        let options = GenerateOptions {
            thinking: Some(ThinkingConfig::EnabledExplicit),
            thinking_keep: Some(true),
            ..Default::default()
        };
        let extra =
            KimiChatProvider::build_extra_body(&options, &KimiModelInfo::builtin("kimi-k2.6"));
        assert_eq!(extra["thinking"]["keep"], json!("all"));
        assert!(
            extra["thinking"]["keep"].is_string(),
            "keep must be a string, got {}",
            extra["thinking"]["keep"]
        );
    }

    /// Disabling Preserved Thinking omits the field. Moonshot has no
    /// `keep: false`, so sending one would be a shape it never documented.
    #[test]
    fn disabling_preserved_thinking_omits_keep_rather_than_sending_false() {
        let options = GenerateOptions {
            thinking: Some(ThinkingConfig::EnabledExplicit),
            thinking_keep: Some(false),
            ..Default::default()
        };
        let extra =
            KimiChatProvider::build_extra_body(&options, &KimiModelInfo::builtin("kimi-k2.6"));
        assert_eq!(extra["thinking"]["type"], json!("enabled"));
        assert!(extra["thinking"].get("keep").is_none());
    }

    /// Every model Kimi serves takes images, as its catalogues list them.
    #[test]
    fn every_kimi_model_takes_images() {
        for model in [
            "kimi-k2.6",
            "Kimi-K2.6",
            "kimi-k2.7-code",
            "kimi-for-coding",
            "kimi-for-coding-highspeed",
            "k3",
            "k3-256k",
        ] {
            assert!(KimiChatProvider::supports_vision_for(model), "{model}");
        }
    }

    /// `kimi-for-coding` is uncatalogued. Declaring it text-only made
    /// `downgrade_unsupported_media` silently replace every image with a text
    /// marker, so an uncatalogued Kimi alias is assumed multimodal: a wrong
    /// `true` produces a provider error, a wrong `false` destroys data quietly.
    #[test]
    fn an_uncatalogued_kimi_alias_is_assumed_multimodal() {
        assert!(KimiChatProvider::supports_vision_for("kimi-for-coding"));
        assert!(KimiChatProvider::supports_vision_for(
            "kimi-some-future-alias"
        ));
        // ...but the assumption does not leak to other vendors' ids.
        assert!(!KimiChatProvider::supports_vision_for("glm-5v-turbo"));
        assert!(!KimiChatProvider::supports_vision_for(""));
    }

    /// Without its catalogue, each endpoint lists the models it serves, each
    /// sized from the window table rather than guessed from its name.
    #[test]
    fn each_endpoint_lists_the_models_it_serves() {
        let ids = |kimi_coding| -> Vec<String> {
            KimiChatProvider::builtin_models(kimi_coding)
                .into_iter()
                .map(|m| m.id)
                .collect()
        };
        assert_eq!(
            ids(true),
            [
                "kimi-for-coding",
                "kimi-for-coding-highspeed",
                "k3",
                "k3-256k",
                "k3.1-pro",
                "k3.1-pro-highspeed"
            ]
        );
        assert_eq!(
            ids(false),
            [
                "kimi-k3.1-pro",
                "kimi-k3.1-pro-highspeed",
                "kimi-k3",
                "kimi-k2.7-code",
                "kimi-k2.7-code-highspeed",
                "kimi-k2.6"
            ]
        );
        for id in ids(true).iter().chain(&ids(false)) {
            assert!(
                KimiChatProvider::model_metadata().contains_key(id.as_str()),
                "{id} is sized by a guess"
            );
        }

        let timeouts = HttpTimeouts::idle(std::time::Duration::from_secs(5));
        let listed = |base_url: &str| -> Vec<String> {
            KimiChatProvider::new("k3.1-pro", None, base_url, timeouts)
                .unwrap()
                .known_models()
                .into_iter()
                .map(|m| m.id)
                .collect()
        };
        assert_eq!(listed("https://api.kimi.ai/coding/v1"), ids(true));
        assert_eq!(listed("https://api.moonshot.ai/v1"), ids(false));
    }

    /// A model the catalogue lists as never reasoning gets no `thinking`
    /// object — an undeclared property would be rejected, and a request to
    /// think cannot be honoured anyway.
    #[test]
    fn models_that_never_reason_get_no_thinking_object() {
        let silent = KimiModelInfo {
            thinking: ThinkingSupport::No,
            efforts: None,
            ..KimiModelInfo::builtin("kimi-plain")
        };
        for thinking in [ThinkingConfig::EnabledExplicit, ThinkingConfig::Disabled] {
            let options = GenerateOptions {
                thinking: Some(thinking),
                thinking_keep: Some(true),
                reasoning_effort: Some("high".to_string()),
                ..Default::default()
            };
            let extra = KimiChatProvider::build_extra_body(&options, &silent);
            assert!(extra.get("thinking").is_none(), "{extra}");
            assert!(extra.get("reasoning_effort").is_none());
        }
    }

    /// An explicit `false` must beat the default, or it cannot be turned off.
    #[test]
    fn preserved_thinking_can_be_disabled_explicitly_on_kimi_models() {
        let options = GenerateOptions {
            thinking: Some(ThinkingConfig::EnabledExplicit),
            thinking_keep: Some(false),
            ..Default::default()
        };
        let extra = KimiChatProvider::build_extra_body(
            &options,
            &KimiModelInfo::builtin("kimi-for-coding"),
        );
        assert_eq!(extra["thinking"]["type"], json!("enabled"));
        assert!(extra["thinking"].get("keep").is_none());
    }

    /// The default only applies when Thinking itself is on: it must not switch
    /// Thinking on for a model that was asked to run without it.
    #[test]
    fn preserved_thinking_default_does_not_enable_thinking_by_itself() {
        let options = GenerateOptions {
            thinking: None,
            thinking_keep: None,
            ..Default::default()
        };
        let extra = KimiChatProvider::build_extra_body(
            &options,
            &KimiModelInfo::builtin("kimi-for-coding"),
        );
        assert!(extra.get("thinking").is_none());
    }

    /// With no cap configured, Kimi keeps `window − input`; an unknown window
    /// sends nothing.
    #[test]
    fn unset_completion_tokens_sends_the_rest_of_the_window() {
        let provider = KimiChatProvider::new(
            "k3",
            None,
            "https://api.kimi.ai/coding/v1",
            HttpTimeouts::idle(std::time::Duration::from_secs(5)),
        )
        .unwrap();
        assert_eq!(
            provider.completion_tokens_when_unset(8_000, 262_144),
            Some(254_144)
        );
        // The model is always entitled to at least one token.
        assert_eq!(
            provider.completion_tokens_when_unset(300_000, 262_144),
            Some(1)
        );
        assert_eq!(provider.completion_tokens_when_unset(0, 0), None);
    }

    /// `kimi-k2.8` ships before the table knows it; it must keep k2.6's 262k
    /// window rather than fall to the 128k default.
    #[test]
    fn uncatalogued_kimi_minor_inherits_the_previous_minors_window() {
        assert_eq!(KimiChatProvider::context_window_for("kimi-k2.8"), 262_144);
        assert_eq!(
            KimiChatProvider::context_window_for("kimi-k2.9-code"),
            262_144,
            "a variant suffix inherits from its own lineage"
        );
    }

    /// The size-suffix heuristics and the default still apply to ids that name
    /// no minor version, and the minor fallback must not shadow them.
    #[test]
    fn models_without_a_minor_keep_the_suffix_heuristics() {
        assert_eq!(
            KimiChatProvider::context_window_for("some-model-1m"),
            1_048_576
        );
        assert_eq!(
            KimiChatProvider::context_window_for("some-model-32k"),
            32_768
        );
        assert_eq!(KimiChatProvider::context_window_for("mystery"), 128_000);
    }

    #[test]
    fn test_context_window_known_models() {
        assert_eq!(KimiChatProvider::context_window_for("kimi-k2.6"), 262_144);
        assert_eq!(
            KimiChatProvider::context_window_for("kimi-k2.7-code"),
            262_144
        );
    }

    /// Moonshot retired these, and they answer 404: they are neither listed
    /// nor sized.
    #[test]
    fn retired_models_are_not_advertised() {
        let listed: Vec<String> = [true, false]
            .into_iter()
            .flat_map(KimiChatProvider::builtin_models)
            .map(|m| m.id)
            .collect();
        for retired in [
            "moonshot-v1-8k",
            "moonshot-v1-128k",
            "kimi-k2.5",
            "kimi-k2-0",
            "kimi-k2-0711-preview",
        ] {
            assert!(!listed.iter().any(|id| id == retired), "{retired}");
            assert!(
                !KimiChatProvider::model_metadata().contains_key(retired),
                "{retired}"
            );
        }
    }

    #[test]
    fn test_context_window_unknown_defaults_to_128k() {
        assert_eq!(
            KimiChatProvider::context_window_for("kimi-unknown"),
            128_000
        );
    }

    #[test]
    fn test_context_window_for_coding_model() {
        assert_eq!(
            KimiChatProvider::context_window_for("kimi-for-coding"),
            1_048_576
        );
    }

    /// K3 is the short-form flagship on the Coding Plan: 1M context, multimodal,
    /// always-thinking, and effort-aware. Verified against the live catalogue.
    #[test]
    fn k3_is_a_one_million_context_multimodal_thinking_model() {
        assert_eq!(KimiChatProvider::context_window_for("k3"), 1_048_576);
        assert!(KimiChatProvider::supports_vision_for("k3"));
        assert!(KimiChatProvider::takes_thinking_keep("k3"));
    }

    fn extra(options: GenerateOptions, model: &str) -> Value {
        KimiChatProvider::build_extra_body(&options, &KimiModelInfo::builtin(model))
    }

    /// K3 takes its effort inside `thinking`, mapped onto its ladder, with
    /// Preserved Thinking alongside — and never as a top-level field.
    #[test]
    fn k3_takes_mapped_effort_in_the_thinking_object() {
        let body = extra(
            GenerateOptions {
                thinking: Some(ThinkingConfig::Enabled),
                reasoning_effort: Some("medium".to_string()),
                ..Default::default()
            },
            "k3",
        );
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "effort": "high", "keep": "all"})
        );
        assert!(body.get("reasoning_effort").is_none());

        for (input, expected) in [("low", "low"), ("high", "high"), ("xhigh", "max")] {
            let body = extra(
                GenerateOptions {
                    reasoning_effort: Some(input.to_string()),
                    ..Default::default()
                },
                "k3",
            );
            assert_eq!(body["thinking"]["effort"], json!(expected), "{input}");
        }

        // An unknown value is left out, so the model's own default applies.
        let body = extra(
            GenerateOptions {
                reasoning_effort: Some("turbo".to_string()),
                ..Default::default()
            },
            "k3",
        );
        assert!(body.get("thinking").is_none(), "{body}");
    }

    /// The two ways to ask for thinking off: directly, or as effort `none`.
    fn off_requests() -> [GenerateOptions; 2] {
        [
            GenerateOptions {
                thinking: Some(ThinkingConfig::Disabled),
                reasoning_effort: Some("high".to_string()),
                ..Default::default()
            },
            GenerateOptions {
                reasoning_effort: Some("none".to_string()),
                ..Default::default()
            },
        ]
    }

    /// Off is sent as `disabled` to a model that takes it — Kimi Code serves
    /// `k3` and the `kimi-for-coding` aliases with K2.8 Preview, without
    /// thinking — and never carries an effort.
    #[test]
    fn thinking_off_is_sent_as_disabled_without_an_effort() {
        for options in off_requests() {
            for model in ["k3", "k3-256k", "kimi-for-coding", "kimi-k2.6"] {
                let body = extra(options.clone(), model);
                assert_eq!(body["thinking"], json!({"type": "disabled"}), "{model}");
                assert!(body.get("reasoning_effort").is_none());
            }
        }
    }

    /// A model that cannot turn thinking off runs the lightest thinking it
    /// has instead. `disabled` would be a 400 on K3.1 Pro, and the configured
    /// effort would make "thinking off" run at `high`.
    #[test]
    fn thinking_off_runs_the_lightest_thinking_where_it_cannot_be_turned_off() {
        for options in off_requests() {
            for model in ["k3.1-pro", "k3.1-pro-highspeed"] {
                assert_eq!(
                    extra(options.clone(), model)["thinking"],
                    json!({"type": "enabled", "effort": "low", "keep": "all"}),
                    "{model}"
                );
            }
            let platform = extra(options.clone(), "kimi-k3.1-pro");
            assert_eq!(platform, json!({"reasoning_effort": "low"}));
            assert_eq!(
                extra(options.clone(), "kimi-k2.7-code")["thinking"],
                json!({"type": "enabled", "keep": "all"}),
                "no effort ladder, so just thinking"
            );
        }
    }

    /// Which models refuse thinking off, from Kimi's models page and the
    /// platform's parameter reference; a later `k` id is taken to as well.
    #[test]
    fn the_models_that_cannot_turn_thinking_off() {
        for (model, expected) in [
            ("k3.1-pro", true),
            ("K3.1-Pro-Highspeed", true),
            ("k3.2", true),
            ("k4", true),
            ("kimi-k3.1-pro", true),
            ("kimi-k3", true),
            ("kimi-k2.7-code", true),
            ("kimi-k2.7-code-highspeed", true),
            ("k3", false),
            ("k3-256k", false),
            ("kimi-for-coding", false),
            ("kimi-for-coding-highspeed", false),
            ("kimi-k2.6", false),
        ] {
            assert_eq!(cannot_turn_thinking_off(model), expected, "{model}");
        }
        assert_eq!(k_series_version("k3.1-pro"), Some((3, 1)));
        assert_eq!(k_series_version("k12.10"), Some((12, 10)));
        assert_eq!(k_series_version("k3-256k"), Some((3, 0)));
        assert_eq!(k_series_version("k3-v1.5"), Some((3, 0)));
        assert_eq!(k_series_version("kimi-k3.1-pro"), None);
    }

    /// The platform's K3 series takes its effort as a top-level
    /// `reasoning_effort`, mapped onto its ladder, and never a `thinking`
    /// object — a K2.x parameter there — even when Preserved Thinking is
    /// asked for: these models always keep their reasoning.
    #[test]
    fn the_platform_k3_series_takes_a_top_level_reasoning_effort() {
        for (input, expected) in [("medium", "high"), ("xhigh", "max"), ("minimal", "low")] {
            for model in ["kimi-k3.1-pro", "kimi-k3.1-pro-highspeed", "kimi-k3"] {
                let body = extra(
                    GenerateOptions {
                        thinking: Some(ThinkingConfig::EnabledExplicit),
                        thinking_keep: Some(true),
                        reasoning_effort: Some(input.to_string()),
                        ..Default::default()
                    },
                    model,
                );
                assert_eq!(
                    body,
                    json!({"reasoning_effort": expected}),
                    "{model} {input}"
                );
            }
        }
        // No effort asked for: nothing is sent, and the model's own default
        // (`max`) applies.
        let body = extra(
            GenerateOptions {
                thinking: Some(ThinkingConfig::Enabled),
                ..Default::default()
            },
            "kimi-k3.1-pro",
        );
        assert_eq!(body, json!({}));
    }

    /// The platform's K3 series bills cache writes by lifetime and is told
    /// the one asked for; Kimi Code and the K2 models, which do not, are
    /// told nothing. Unset, the field is left out and the platform writes for
    /// five minutes.
    #[test]
    fn the_cache_lifetime_goes_to_the_platforms_k3_series_only() {
        use crate::provider::PromptCacheTtl;
        let hour = GenerateOptions {
            prompt_cache_ttl: Some(PromptCacheTtl::OneHour),
            ..Default::default()
        };
        for model in ["kimi-k3.1-pro", "kimi-k3.1-pro-highspeed", "kimi-k3"] {
            assert_eq!(
                extra(hour.clone(), model)["prompt_cache_options"],
                json!({"mode": "implicit", "ttl": "1h"}),
                "{model}"
            );
        }
        let five = GenerateOptions {
            prompt_cache_ttl: Some(PromptCacheTtl::FiveMinutes),
            ..Default::default()
        };
        assert_eq!(
            extra(five, "kimi-k3.1-pro")["prompt_cache_options"]["ttl"],
            json!("5m")
        );
        for model in ["k3.1-pro", "kimi-for-coding", "kimi-k2.7-code", "kimi-k2.6"] {
            assert!(
                extra(hour.clone(), model)
                    .get("prompt_cache_options")
                    .is_none(),
                "{model}"
            );
        }
        assert!(
            extra(GenerateOptions::default(), "kimi-k3.1-pro")
                .get("prompt_cache_options")
                .is_none()
        );
        assert_eq!("1H".parse(), Ok(PromptCacheTtl::OneHour));
        assert!("1d".parse::<PromptCacheTtl>().is_err());
    }

    /// A configured temperature or top-p never reaches Kimi, which fixes
    /// both and answers any other value with a 400.
    #[tokio::test]
    async fn fixed_sampling_settings_are_not_sent() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-test",
                "choices": [{ "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })))
            .mount(&server)
            .await;
        let provider = KimiChatProvider::new(
            "k3.1-pro",
            Some("sk-test".to_string()),
            server.uri(),
            HttpTimeouts::idle(std::time::Duration::from_secs(5)),
        )
        .unwrap();
        provider
            .generate(
                None,
                &[],
                &[Message::user("hi")],
                GenerateOptions {
                    temperature: Some(0.3),
                    top_p: Some(0.5),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(body.get("temperature").is_none(), "{body}");
        assert!(body.get("top_p").is_none(), "{body}");
    }

    /// The effort ladder follows the model: `kimi-for-coding` (K2.8) has one;
    /// the highspeed alias and the K2.x ids do not.
    #[test]
    fn only_models_with_a_ladder_receive_effort() {
        let with = extra(
            GenerateOptions {
                reasoning_effort: Some("max".to_string()),
                ..Default::default()
            },
            "kimi-for-coding",
        );
        assert_eq!(with["thinking"]["effort"], json!("max"));
        for model in ["kimi-for-coding-highspeed", "kimi-k2.7-code", "kimi-k2.6"] {
            let body = extra(
                GenerateOptions {
                    reasoning_effort: Some("high".to_string()),
                    ..Default::default()
                },
                model,
            );
            assert!(
                body.get("thinking").and_then(|t| t.get("effort")).is_none(),
                "{model} must not receive an effort"
            );
        }
    }

    /// A catalogue entry overrides the built-in table, including a narrower
    /// effort ladder.
    #[test]
    fn a_catalogue_entry_overrides_the_builtin_table() {
        use super::super::kimi_catalog::EffortLadder;
        let timeouts = HttpTimeouts::idle(std::time::Duration::from_secs(5));
        let catalogue = vec![KimiModelInfo {
            id: "kimi-for-coding".to_string(),
            display_name: Some("K2.9".to_string()),
            context_length: 2_097_152,
            thinking: ThinkingSupport::Only,
            image_in: true,
            video_in: false,
            dynamic_tools: true,
            efforts: Some(EffortLadder {
                valid: vec!["low".into(), "high".into()],
                default: Some("high".into()),
            }),
        }];
        let provider = KimiChatProvider::new(
            "kimi-for-coding",
            Some("k".to_string()),
            "https://api.kimi.com/coding/v1",
            timeouts,
        )
        .unwrap()
        .with_catalog(catalogue);
        assert_eq!(provider.context_window(), 2_097_152);
        assert!(!provider.media_policy().takes_video());
        assert_eq!(provider.known_models().len(), 1);
        let body = KimiChatProvider::build_extra_body(
            &GenerateOptions {
                reasoning_effort: Some("max".to_string()),
                ..Default::default()
            },
            &provider.info,
        );
        assert_eq!(
            body["thinking"]["effort"],
            json!("high"),
            "nearest accepted level"
        );
    }

    /// The Kimi Code models' documented windows, and the `-256k` suffix rule.
    #[test]
    fn the_kimi_coding_models_have_their_documented_windows() {
        assert_eq!(
            KimiChatProvider::context_window_for("kimi-for-coding"),
            1_048_576
        );
        assert_eq!(
            KimiChatProvider::context_window_for("kimi-for-coding-highspeed"),
            262_144
        );
        assert_eq!(KimiChatProvider::context_window_for("k3-256k"), 262_144);
        assert_eq!(KimiChatProvider::context_window_for("k4-256k"), 262_144);
        assert!(KimiChatProvider::supports_vision_for("k3-256k"));
        assert!(!KimiChatProvider::supports_video_for("k3-256k"));
    }

    #[test]
    fn test_context_window_infers_from_suffix() {
        assert_eq!(KimiChatProvider::context_window_for("kimi-foo-8k"), 8_192);
        assert_eq!(
            KimiChatProvider::context_window_for("kimi-foo-128k"),
            131_072
        );
        assert_eq!(
            KimiChatProvider::context_window_for("kimi-foo-1m"),
            1_048_576
        );
    }

    #[tokio::test]
    async fn test_generate_serializes_builtin_web_search() {
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

        let provider = KimiChatProvider::new(
            "kimi-k2.6",
            Some("sk-test".to_string()),
            server.uri(),
            HttpTimeouts::idle(Duration::from_secs(5)),
        )
        .unwrap();

        let builtins = provider.builtin_tools();
        let _ = provider
            .generate(None, &builtins, &[], GenerateOptions::default())
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], json!("builtin_function"));
        assert_eq!(tools[0]["function"]["name"], json!("$web_search"));
        assert!(tools[0]["function"].get("description").is_none());
        assert!(tools[0]["function"].get("parameters").is_none());
    }

    /// A model that takes tools mid-conversation gets them on the system
    /// message, normalized like the top-level list; one that does not, or a
    /// generic endpoint, never sees the field.
    #[tokio::test]
    async fn message_level_tools_go_only_to_dynamic_models() {
        use std::time::Duration;

        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-test",
                "choices": [{ "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })))
            .mount(&server)
            .await;
        let delivered = Message {
            role: crate::message::Role::System,
            content: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: vec![Tool::new(
                "mcp__docs__lookup",
                "Look something up.",
                // A `$ref` Kimi cannot read, normalized away before sending.
                json!({
                    "type": "object",
                    "properties": { "q": { "$ref": "#/$defs/query" } },
                    "$defs": { "query": { "type": "string" } }
                }),
            )],
            partial: false,
        };
        let history = [Message::user("find x"), delivered];
        let timeouts = HttpTimeouts::idle(Duration::from_secs(5));
        let body_of = |n: usize, requests: &[wiremock::Request]| -> Value {
            serde_json::from_slice(&requests[n].body).unwrap()
        };

        let dynamic =
            KimiChatProvider::new("k3", Some("sk-test".into()), server.uri(), timeouts).unwrap();
        assert!(dynamic.supports_dynamic_tools());
        dynamic
            .generate(None, &[], &history, GenerateOptions::default())
            .await
            .unwrap();

        let fixed = KimiChatProvider::new(
            "kimi-for-coding-highspeed",
            Some("sk-test".into()),
            server.uri(),
            timeouts,
        )
        .unwrap();
        assert!(!fixed.supports_dynamic_tools());
        fixed
            .generate(None, &[], &history, GenerateOptions::default())
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let sent = body_of(0, &requests);
        let system = &sent["messages"][1];
        assert_eq!(system["role"], json!("system"));
        let parameters = &system["tools"][0]["function"]["parameters"];
        assert_eq!(
            parameters["properties"]["q"]["type"],
            json!("string"),
            "{sent}"
        );
        assert!(parameters.get("$defs").is_none(), "{sent}");

        let stripped = body_of(1, &requests);
        let messages = stripped["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            1,
            "the tools-only message is dropped: {stripped}"
        );
        assert!(messages.iter().all(|m| m.get("tools").is_none()));
    }

    mod file_uploads {
        use std::time::Duration;

        use serde_json::{Value, json};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        use super::super::*;
        use crate::message::{ImageUrl, UrlRef};

        fn answer() -> ResponseTemplate {
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-test",
                "choices": [{
                    "message": { "role": "assistant", "content": "red" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            }))
        }

        fn provider(server: &MockServer) -> KimiChatProvider {
            KimiChatProvider::new(
                "kimi-for-coding",
                Some("sk-test".to_string()),
                server.uri(),
                HttpTimeouts::idle(Duration::from_secs(5)),
            )
            .unwrap()
            .with_file_uploads(true)
        }

        fn with_media(parts: Vec<ContentPart>) -> Vec<Message> {
            let mut message = Message::user("");
            message.content = parts;
            vec![message]
        }

        fn image(url: &str) -> ContentPart {
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: url.to_string(),
                    id: None,
                },
            }
        }

        fn video(url: &str) -> ContentPart {
            ContentPart::VideoUrl {
                video_url: UrlRef {
                    url: url.to_string(),
                    id: None,
                },
            }
        }

        async fn chat_bodies(server: &MockServer) -> Vec<Value> {
            server
                .received_requests()
                .await
                .unwrap()
                .into_iter()
                .filter(|r| r.url.path() == "/chat/completions")
                .map(|r| serde_json::from_slice(&r.body).unwrap())
                .collect()
        }

        async fn uploads(server: &MockServer) -> usize {
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|r| r.url.path() == "/files")
                .count()
        }

        /// Media goes up once and is sent by reference from then on.
        #[tokio::test]
        async fn images_and_video_are_uploaded_once_and_sent_by_reference() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/files"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "file-1"})))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(answer())
                .mount(&server)
                .await;
            let provider = provider(&server);
            let history = with_media(vec![image("data:image/png;base64,iVBORw0KGgo=")]);

            for _ in 0..2 {
                provider
                    .generate(None, &[], &history, GenerateOptions::default())
                    .await
                    .unwrap();
            }
            assert_eq!(uploads(&server).await, 1);
            let bodies = chat_bodies(&server).await;
            for body in &bodies {
                assert_eq!(
                    body["messages"][0]["content"][0]["image_url"]["url"],
                    "ms://file-1"
                );
            }
            assert_eq!(
                provider.media_policy().video,
                VideoSupport::Upload,
                "kimi-for-coding takes video through uploads"
            );
        }

        /// A remembered upload Kimi no longer has is uploaded again, once.
        #[tokio::test]
        async fn a_stale_upload_is_replaced() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/files"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "file-1"})))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/files"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "file-2"})))
                .mount(&server)
                .await;
            // The first chat succeeds; the second names the stale id and gets a
            // 404; the retry with the fresh upload succeeds.
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(answer())
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(404).set_body_string(
                    r#"{"error":{"message":"The requested resource was not found","type":"resource_not_found_error"}}"#,
                ))
                .up_to_n_times(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(answer())
                .mount(&server)
                .await;
            let provider = provider(&server);
            let history = with_media(vec![image("data:image/png;base64,iVBORw0KGgo=")]);

            provider
                .generate(None, &[], &history, GenerateOptions::default())
                .await
                .unwrap();
            provider
                .generate(None, &[], &history, GenerateOptions::default())
                .await
                .expect("the stale upload is replaced and the request retried");
            assert_eq!(uploads(&server).await, 2);
            let bodies = chat_bodies(&server).await;
            assert_eq!(
                bodies.last().unwrap()["messages"][0]["content"][0]["image_url"]["url"],
                "ms://file-2"
            );
        }

        /// A failed upload leaves an image inline and turns a video into a
        /// note; an authentication failure surfaces.
        #[tokio::test]
        async fn failed_uploads_fall_back() {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/files"))
                .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(answer())
                .mount(&server)
                .await;
            let kimi = provider(&server);
            let history = with_media(vec![
                image("data:image/png;base64,iVBORw0KGgo="),
                video("data:video/mp4;base64,AAAA"),
            ]);
            kimi.generate(None, &[], &history, GenerateOptions::default())
                .await
                .unwrap();
            let content = &chat_bodies(&server).await[0]["messages"][0]["content"];
            assert_eq!(
                content[0]["image_url"]["url"],
                "data:image/png;base64,iVBORw0KGgo="
            );
            assert!(
                content[1]["text"]
                    .as_str()
                    .unwrap()
                    .starts_with("[video omitted: uploading it to Kimi's Files API failed")
            );

            let denied = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/files"))
                .respond_with(ResponseTemplate::new(401).set_body_string("{}"))
                .mount(&denied)
                .await;
            let err = provider(&denied)
                .generate(None, &[], &history, GenerateOptions::default())
                .await
                .err()
                .expect("an authentication failure surfaces");
            assert!(kimi_files::is_auth_error(&err), "{err}");
        }

        /// A host that refuses the key hands the request to the other host;
        /// once that works, requests go there directly.
        #[tokio::test]
        async fn a_refused_key_is_tried_on_the_other_host() {
            let refusing = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(401).set_body_string(
                    r#"{"error":{"message":"Invalid Authentication","type":"invalid_authentication_error"}}"#,
                ))
                .mount(&refusing)
                .await;
            let accepting = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(answer())
                .mount(&accepting)
                .await;
            let timeouts = HttpTimeouts::idle(Duration::from_secs(5));
            let kimi = KimiChatProvider::new(
                "kimi-for-coding",
                Some("sk-test".to_string()),
                refusing.uri(),
                timeouts,
            )
            .unwrap()
            .with_alternate_host(&accepting.uri(), timeouts)
            .unwrap();

            for _ in 0..2 {
                kimi.generate(
                    None,
                    &[],
                    &[Message::user("hi")],
                    GenerateOptions::default(),
                )
                .await
                .expect("the other host accepts the key");
            }
            assert_eq!(refusing.received_requests().await.unwrap().len(), 1);
            assert_eq!(accepting.received_requests().await.unwrap().len(), 2);

            // Without an alternate, the refusal surfaces.
            let alone = KimiChatProvider::new(
                "kimi-for-coding",
                Some("sk-test".to_string()),
                refusing.uri(),
                timeouts,
            )
            .unwrap();
            let err = alone
                .generate(
                    None,
                    &[],
                    &[Message::user("hi")],
                    GenerateOptions::default(),
                )
                .await
                .err()
                .expect("refused");
            assert!(is_refused_key(&err), "{err}");
        }

        /// A server that answers every chat completion with `status` and
        /// `message`.
        async fn answering(status: u16, message: &str) -> MockServer {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .respond_with(ResponseTemplate::new(status).set_body_json(
                    json!({"error": {"message": message, "type": "invalid_request_error"}}),
                ))
                .mount(&server)
                .await;
            server
        }

        async fn error_from(first: &MockServer, other: &MockServer) -> LlmError {
            let timeouts = HttpTimeouts::idle(Duration::from_secs(5));
            KimiChatProvider::new("k3", Some("sk-test".to_string()), first.uri(), timeouts)
                .unwrap()
                .with_alternate_host(&other.uri(), timeouts)
                .unwrap()
                .generate(
                    None,
                    &[],
                    &[Message::user("hi")],
                    GenerateOptions::default(),
                )
                .await
                .err()
                .expect("turned down")
        }

        /// Kimi answers `401` when a valid key's plan does not cover the
        /// request. That comes from the host holding the key, so it surfaces
        /// as it is, and the other host is not asked.
        #[tokio::test]
        async fn a_plan_limit_is_not_taken_for_a_refused_key() {
            let limited = answering(
                401,
                "Your current subscription does not have access to k3. Upgrade to \
                 higher-tier Kimi Code plans.",
            )
            .await;
            let other = answering(200, "unused").await;
            let err = error_from(&limited, &other).await;
            assert!(
                err.to_string().contains("does not have access to k3"),
                "{err}"
            );
            assert!(other.received_requests().await.unwrap().is_empty());
        }

        /// When the other host takes the key but turns the request down, its
        /// answer is reported; when it refuses the key as well, the first
        /// host's refusal is.
        #[tokio::test]
        async fn the_answer_that_explains_the_failure_is_reported() {
            let refusing = answering(401, "Invalid Authentication").await;
            let limited = answering(
                401,
                "Your current plan supports only k3 up to 256K context.",
            )
            .await;
            let err = error_from(&refusing, &limited).await;
            assert!(
                err.to_string().contains("supports only k3 up to 256K"),
                "{err}"
            );

            let also_refusing = answering(
                401,
                "The API Key appears to be invalid or may have expired.",
            )
            .await;
            let err = error_from(&refusing, &also_refusing).await;
            assert!(err.to_string().contains("Invalid Authentication"), "{err}");
        }

        /// Uploads are on for the Kimi Code endpoints only.
        #[test]
        fn uploads_follow_the_endpoint() {
            assert!(is_kimi_coding_endpoint("https://api.kimi.com/coding/v1"));
            assert!(is_kimi_coding_endpoint("https://api.kimi.ai/coding/v1"));
            assert!(!is_kimi_coding_endpoint("https://api.moonshot.ai/v1"));
            assert!(!is_kimi_coding_endpoint("http://127.0.0.1:9/coding/v1"));
            let timeouts = HttpTimeouts::idle(Duration::from_secs(5));
            let platform = KimiChatProvider::new(
                "kimi-k2.7-code",
                Some("k".to_string()),
                "https://api.moonshot.ai/v1",
                timeouts,
            )
            .unwrap();
            assert!(!platform.media_policy().takes_video());
            let code = KimiChatProvider::new(
                "k3-256k",
                Some("k".to_string()),
                "https://api.kimi.com/coding/v1",
                timeouts,
            )
            .unwrap();
            assert!(code.media_policy().accepts_image("image/heic"));
            assert!(!code.media_policy().takes_video(), "k3-256k takes no video");
        }
    }

    /// The catalogue's answer must reach the inner provider, or every Kimi
    /// model would be judged by an OpenAI-shaped name guess — `false` for all
    /// of them — and images would be silently stripped from models that see.
    #[test]
    fn the_catalogue_vision_capability_reaches_the_wire_layer() {
        let timeouts = HttpTimeouts::idle(std::time::Duration::from_secs(5));
        let seeing = KimiChatProvider::new(
            "kimi-k3".to_string(),
            Some("k".to_string()),
            "https://api.kimi.com/coding/v1".to_string(),
            timeouts,
        )
        .unwrap();
        assert!(
            crate::provider::ChatProvider::supports_vision(&seeing),
            "k3 is multimodal"
        );

        let blind = KimiChatProvider::new(
            "k3".to_string(),
            Some("k".to_string()),
            "https://api.kimi.com/coding/v1".to_string(),
            timeouts,
        )
        .unwrap()
        .with_catalog(vec![KimiModelInfo {
            image_in: false,
            video_in: false,
            ..KimiModelInfo::builtin("k3")
        }]);
        assert!(
            !crate::provider::ChatProvider::supports_vision(&blind),
            "the catalogue says this k3 takes no images"
        );
    }
}
