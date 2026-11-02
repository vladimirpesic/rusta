use serde::{Deserialize, Serialize};

/// Token usage reported by a provider.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Moonshot / GLM cache-read tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    /// GLM reasoning tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Provider-reported prompt cache read tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_cache_read: Option<u64>,
    /// Provider-reported prompt cache creation / write tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_cache_creation: Option<u64>,
}

impl TokenUsage {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn add(&mut self, other: &Self) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.total_tokens += other.total_tokens;
        self.cached_tokens = add_opt(self.cached_tokens, other.cached_tokens);
        self.reasoning_tokens = add_opt(self.reasoning_tokens, other.reasoning_tokens);
        self.input_cache_read = add_opt(self.input_cache_read, other.input_cache_read);
        self.input_cache_creation = add_opt(self.input_cache_creation, other.input_cache_creation);
    }
}

fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}
