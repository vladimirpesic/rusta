//! Error taxonomy for `rusta-llm` — development plan §6.11.
//!
//! Every variant that can reach the model or the user carries an actionable
//! remedy in its message.

/// Errors surfaced by the backend layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 404 on `/chat/completions`: `base_url` is not the full API root (plan §6.2).
    #[error(
        "404 Not Found at {url}. `base_url` must be the full API root including /v1. \
         Common forms: llama.cpp llama-server http://127.0.0.1:8080/v1 · \
         Ollama http://127.0.0.1:11434/v1 · LM Studio http://127.0.0.1:1234/v1"
    )]
    BaseUrl404 {
        /// The failing endpoint URL.
        url: String,
    },

    /// Non-retryable HTTP status (4xx other than 404) with a clipped server message.
    #[error("HTTP {status} from server: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Server-provided detail, clipped to 10 lines (plan §6.11).
        message: String,
    },

    /// The embedded backend failed to load the GGUF model (plan §6.2, §6.11).
    #[error(
        "failed to load GGUF model {path}: {cause}. Remedy: verify the path and file integrity; \
         keep gpu_layers = 0 unless llama.cpp was built with GPU support (embedded-cuda)"
    )]
    ModelLoad {
        /// The model path as configured.
        path: String,
        /// Underlying load failure.
        cause: String,
    },

    /// Embedded inference failed — template render, tokenization, or decode
    /// (plan §6.2). Mid-stream failures are surfaced as [`crate::StreamEvent::Failed`].
    #[error("embedded inference failed: {cause}")]
    Inference {
        /// What failed, with a remedy where one exists.
        cause: String,
    },

    /// All retry attempts failed — connection errors or 5xx (plan §6.2: 3 attempts).
    #[error(
        "backend unreachable after {attempts} attempts: {cause}. \
         Remedy: is the server running, and is `base_url` the full API root incl. /v1?"
    )]
    Unreachable {
        /// Number of attempts made.
        attempts: u32,
        /// Last observed failure.
        cause: String,
    },

    /// A stream broke after partial output was already delivered (no retry mid-stream).
    #[error("stream failed after partial output: {cause}")]
    StreamFailed {
        /// Underlying transport failure.
        cause: String,
    },

    /// The server sent a payload Rusta could not parse.
    #[error("malformed response from server: {cause}")]
    Malformed {
        /// What was wrong with the payload.
        cause: String,
    },

    /// Invalid backend configuration.
    #[error("invalid backend configuration: {cause}")]
    Config {
        /// What was wrong.
        cause: String,
    },
}

/// Clips `text` to at most `max_lines` lines, appending an ellipsis marker when
/// clipping occurred (plan §6.11: wrapped errors truncated to 10 lines).
pub(crate) fn clip_lines(text: &str, max_lines: usize) -> String {
    let total = text.lines().count();
    let mut kept: Vec<&str> = text.lines().take(max_lines).collect();
    if kept.len() < total {
        kept.push("… [truncated]");
    }
    kept.join("\n")
}

#[cfg(test)]
mod tests {
    use super::clip_lines;

    #[test]
    fn clips_and_marks_truncation() {
        assert_eq!(clip_lines("a\nb\nc\nd", 2), "a\nb\n… [truncated]");
    }

    #[test]
    fn keeps_short_text_verbatim() {
        assert_eq!(clip_lines("a\nb", 10), "a\nb");
    }
}
