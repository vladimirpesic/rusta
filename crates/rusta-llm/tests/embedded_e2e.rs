//! Real-GGUF e2e tests for the embedded backend — ADR §9.
//!
//! `#[ignore]`d: they need a real model file selected via the `RUSTA_TEST_GGUF`
//! environment variable, so default runs never depend on model availability
//! (ADR §9: embedded tests are `#[cfg(feature = "embedded")]` + `#[ignore]`,
//! so CI default runs never need cmake). Run with:
//!
//! ```text
//! RUSTA_TEST_GGUF=/path/to/model.gguf \
//!   cargo test -p rusta-llm --features embedded --test embedded_e2e -- --ignored --nocapture
//! ```
#![cfg(feature = "embedded")]

use rusta_llm::embedded::{EmbeddedBackend, EmbeddedConfig};
use rusta_llm::types::{ChatRequest, FinishReason, Message, StreamEvent};

/// Backend over the GGUF named by `RUSTA_TEST_GGUF`, context capped at 2048
/// tokens so large models stay cheap to test with.
fn backend() -> EmbeddedBackend {
    let path = std::env::var("RUSTA_TEST_GGUF")
        .expect("set RUSTA_TEST_GGUF to the path of a real GGUF model");
    let config = EmbeddedConfig {
        ctx_size: Some(2048),
        ..EmbeddedConfig::new(path)
    };
    EmbeddedBackend::new(config).expect("embedded backend initialized")
}

#[test]
#[ignore = "requires a real GGUF model (set RUSTA_TEST_GGUF)"]
fn exact_token_counts_come_from_the_model_tokenizer() {
    let backend = backend();
    assert_eq!(backend.count_tokens(""), 0);
    assert!(backend.count_tokens("fn main() { println!(\"hi\"); }") > 0);
    assert!(backend.context_window() > 0);
}

#[tokio::test]
#[ignore = "requires a real GGUF model (set RUSTA_TEST_GGUF)"]
async fn complete_returns_model_text() {
    let backend = backend();
    let request = ChatRequest {
        max_tokens: 64,
        ..ChatRequest::new(vec![Message::user("Reply with the single word: ok")])
    };
    let text = backend
        .complete(request)
        .await
        .expect("completion succeeds");
    assert!(!text.trim().is_empty(), "model produced no text: {text:?}");
}

#[tokio::test]
#[ignore = "requires a real GGUF model (set RUSTA_TEST_GGUF)"]
async fn streams_deltas_then_finishes() {
    let backend = backend();
    let request = ChatRequest {
        max_tokens: 64,
        ..ChatRequest::new(vec![Message::user("Count from 1 to 5")])
    };
    let mut events = backend.stream(request).expect("stream starts");
    let mut text = String::new();
    let mut finish = None;
    while let Some(event) = events.recv().await {
        match event {
            StreamEvent::Delta(delta) => text.push_str(&delta),
            StreamEvent::ToolCall { .. } => panic!("embedded backend never emits tool calls"),
            StreamEvent::Finish(reason) => {
                finish = Some(reason);
                break;
            }
            StreamEvent::Failed(cause) => panic!("stream failed: {cause}"),
        }
    }
    let finish = finish.expect("stream ended with Finish");
    assert!(
        finish == FinishReason::Stop || finish == FinishReason::Length,
        "unexpected finish reason: {finish:?}"
    );
    assert!(!text.is_empty(), "no deltas were received");
    assert!(events.try_recv().is_err(), "channel closes after Finish");
}
