//! Which wire field an OpenAI-compatible endpoint uses for reasoning content.
//!
//! The OpenAI-compatible ecosystem never agreed on one name. Moonshot, Z.AI and
//! DeepSeek send and accept `reasoning_content`; OpenAI's GPT-OSS guidance and
//! current vLLM use `reasoning`, and vLLM accepts *only* `reasoning` on the
//! request side; OpenRouter-style gateways carry `reasoning_details`.
//!
//! Reading one name and writing another loses thinking in both directions: an
//! endpoint whose reasoning Rusta cannot read produces no `Think` parts at
//! all, and reasoning echoed back under a key the endpoint ignores never reaches
//! the model — which silently defeats Preserved Thinking. So the key is
//! *detected* from what an endpoint sends, remembered per provider, and used for
//! what Rusta sends back.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// The wire field carrying reasoning content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningKey {
    /// `reasoning_content` — Moonshot/Kimi, Z.AI GLM, DeepSeek, older vLLM.
    /// The default, because it is what Rusta's two primary providers require.
    #[default]
    ReasoningContent,
    /// `reasoning` — OpenAI GPT-OSS guidance and current vLLM, which accepts
    /// nothing else on the request side.
    Reasoning,
    /// `reasoning_details` — OpenRouter-style gateways.
    ReasoningDetails,
}

impl ReasoningKey {
    /// The wire field name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReasoningContent => "reasoning_content",
            Self::Reasoning => "reasoning",
            Self::ReasoningDetails => "reasoning_details",
        }
    }

    /// Parse a configured key. Accepts the wire names and their camelCase
    /// spelling (`reasoningKey`).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().replace(['-', '_'], "").as_str() {
            "reasoningcontent" => Some(Self::ReasoningContent),
            "reasoning" => Some(Self::Reasoning),
            "reasoningdetails" => Some(Self::ReasoningDetails),
            _ => None,
        }
    }

    fn from_u8(raw: u8) -> Self {
        match raw {
            1 => Self::Reasoning,
            2 => Self::ReasoningDetails,
            _ => Self::ReasoningContent,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Self::ReasoningContent => 0,
            Self::Reasoning => 1,
            Self::ReasoningDetails => 2,
        }
    }
}

/// The reasoning dialect a single provider has settled on.
///
/// Shared across every request that provider makes, so a dialect learned on the
/// first response is used by every request after it.
#[derive(Debug)]
pub struct ReasoningDialect {
    key: AtomicU8,
    /// Set when the user named a key explicitly. A pinned dialect ignores
    /// detection entirely — an operator who has told Rusta what their gateway
    /// speaks should not be overruled by a stray response from behind it.
    pinned: AtomicBool,
}

impl ReasoningDialect {
    /// A dialect that starts at `reasoning_content` and adapts to what it sees.
    #[must_use]
    pub fn detecting() -> Self {
        Self {
            key: AtomicU8::new(ReasoningKey::default().as_u8()),
            pinned: AtomicBool::new(false),
        }
    }

    /// A dialect fixed to `key`, immune to detection.
    #[must_use]
    pub fn pinned(key: ReasoningKey) -> Self {
        Self {
            key: AtomicU8::new(key.as_u8()),
            pinned: AtomicBool::new(true),
        }
    }

    /// The key to write reasoning under.
    #[must_use]
    pub fn get(&self) -> ReasoningKey {
        ReasoningKey::from_u8(self.key.load(Ordering::Relaxed))
    }

    /// Whether this dialect was pinned by configuration.
    #[must_use]
    pub fn is_pinned(&self) -> bool {
        self.pinned.load(Ordering::Relaxed)
    }

    /// Record the key an endpoint just used. A no-op on a pinned dialect.
    pub fn observe(&self, key: ReasoningKey) {
        if self.is_pinned() {
            return;
        }
        self.key.store(key.as_u8(), Ordering::Relaxed);
    }
}

impl Default for ReasoningDialect {
    fn default() -> Self {
        Self::detecting()
    }
}

/// Pick the reasoning text from the three dialects, in priority order, and
/// report which key carried it.
///
/// `reasoning_content` first because it is what Rusta's primary providers
/// send; `reasoning_details` before `reasoning`, the richer form first. A
/// field present but not a string (an OpenRouter `reasoning_details` array) is
/// skipped rather than failing the message — the next dialect may still carry
/// usable text.
#[must_use]
pub fn pick<'a>(
    reasoning_content: Option<&'a str>,
    reasoning_details: Option<&'a str>,
    reasoning: Option<&'a str>,
) -> Option<(ReasoningKey, &'a str)> {
    reasoning_content
        .map(|t| (ReasoningKey::ReasoningContent, t))
        .or_else(|| reasoning_details.map(|t| (ReasoningKey::ReasoningDetails, t)))
        .or_else(|| reasoning.map(|t| (ReasoningKey::Reasoning, t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_what_kimi_and_glm_require() {
        assert_eq!(ReasoningKey::default(), ReasoningKey::ReasoningContent);
        assert_eq!(ReasoningKey::default().as_str(), "reasoning_content");
        assert_eq!(ReasoningDialect::detecting().get(), ReasoningKey::default());
    }

    #[test]
    fn every_key_round_trips_through_its_wire_name() {
        for key in [
            ReasoningKey::ReasoningContent,
            ReasoningKey::Reasoning,
            ReasoningKey::ReasoningDetails,
        ] {
            assert_eq!(ReasoningKey::parse(key.as_str()), Some(key));
        }
    }

    /// The camelCase spelling `reasoningKey` is accepted too.
    #[test]
    fn parsing_accepts_camel_case_and_is_case_insensitive() {
        assert_eq!(
            ReasoningKey::parse("reasoningContent"),
            Some(ReasoningKey::ReasoningContent)
        );
        assert_eq!(
            ReasoningKey::parse("  REASONING_DETAILS  "),
            Some(ReasoningKey::ReasoningDetails)
        );
        assert_eq!(ReasoningKey::parse("nonsense"), None);
    }

    #[test]
    fn a_detecting_dialect_follows_what_it_sees() {
        let dialect = ReasoningDialect::detecting();
        dialect.observe(ReasoningKey::Reasoning);
        assert_eq!(dialect.get(), ReasoningKey::Reasoning);
        dialect.observe(ReasoningKey::ReasoningContent);
        assert_eq!(dialect.get(), ReasoningKey::ReasoningContent);
    }

    /// An operator who named their gateway's dialect must not be overruled by a
    /// response from behind it.
    #[test]
    fn a_pinned_dialect_ignores_detection() {
        let dialect = ReasoningDialect::pinned(ReasoningKey::Reasoning);
        assert!(dialect.is_pinned());
        dialect.observe(ReasoningKey::ReasoningContent);
        assert_eq!(dialect.get(), ReasoningKey::Reasoning);
    }

    #[test]
    fn pick_prefers_reasoning_content_then_details_then_reasoning() {
        assert_eq!(
            pick(Some("a"), Some("b"), Some("c")),
            Some((ReasoningKey::ReasoningContent, "a"))
        );
        assert_eq!(
            pick(None, Some("b"), Some("c")),
            Some((ReasoningKey::ReasoningDetails, "b"))
        );
        assert_eq!(
            pick(None, None, Some("c")),
            Some((ReasoningKey::Reasoning, "c"))
        );
        assert_eq!(pick(None, None, None), None);
    }
}
