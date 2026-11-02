//! Describing images for a model that cannot see them.
//!
//! When the configured model does not take image input, `read_media` hands
//! each image to a [`VisionFallback`] and passes on the description: Z.AI's
//! vision MCP server for GLM's text models, or a vision model of the same
//! vendor asked once ([`LlmVisionFallback`], DeepSeek's `deepseek-flash` for
//! `deepseek-v4-pro`).

use std::sync::Arc;

use async_trait::async_trait;
use lm_bridge::message::{ContentPart, ImageUrl, Message};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::llm::{Llm, LlmCallbacks};

/// Describes an image for models that do not have native vision support.
#[async_trait]
pub trait VisionFallback: Send + Sync {
    /// Return a textual description of the image, or, when the reading model
    /// asked `question`, the answer to it.
    ///
    /// `mime` is the image MIME type (e.g. `image/png`). `base64_data` is the
    /// raw base64 payload *without* the `data:image/...;base64,` prefix.
    async fn describe_image(
        &self,
        mime: &str,
        base64_data: &str,
        question: Option<&str>,
    ) -> Result<String, String>;
}

/// What a vision model is asked about an image.
const DESCRIBE_PROMPT: &str = "Describe this image in detail for someone who cannot see it, \
     including any text, code, UI elements, or other visible content. Answer with the \
     description only.";

/// The request a vision model gets: `describe`, its own request to describe
/// the image, or the reading model's `question` when it asked one.
#[must_use]
pub fn vision_request(describe: &str, question: Option<&str>) -> String {
    match question.map(str::trim).filter(|q| !q.is_empty()) {
        None => describe.to_string(),
        Some(question) => format!(
            "{question}\n\nAnswer from this image for someone who cannot see it. Quote any \
             text the answer rests on exactly as it is written."
        ),
    }
}

/// Describes an image by asking a vision model once.
pub struct LlmVisionFallback {
    llm: Arc<dyn Llm>,
}

impl LlmVisionFallback {
    /// Ask `llm`, which must take images, to describe each one.
    #[must_use]
    pub fn new(llm: Arc<dyn Llm>) -> Self {
        Self { llm }
    }
}

#[async_trait]
impl VisionFallback for LlmVisionFallback {
    async fn describe_image(
        &self,
        mime: &str,
        base64_data: &str,
        question: Option<&str>,
    ) -> Result<String, String> {
        let mut request = Message::user(vision_request(DESCRIBE_PROMPT, question));
        request.content.push(ContentPart::ImageUrl {
            image_url: ImageUrl {
                url: format!("data:{mime};base64,{base64_data}"),
                id: None,
            },
        });
        let model = self.llm.model_name();
        let step = self
            .llm
            .chat(
                vec![request],
                Vec::new(),
                LlmCallbacks::default(),
                &CancellationToken::new(),
            )
            .await
            .map_err(|e| format!("{model} could not describe the image: {e}"))?;
        let description = step.message.text_content();
        if description.trim().is_empty() {
            return Err(format!("{model} returned no description of the image"));
        }
        Ok(description)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use lm_bridge::errors::LlmError;
    use lm_bridge::tool::Tool;

    use super::*;
    use crate::agent_loop::types::StepResult;

    /// A vision model that answers `answer` and keeps the request it got.
    struct Describer {
        answer: Result<&'static str, ()>,
        seen: Mutex<Vec<Message>>,
    }

    #[async_trait]
    impl Llm for Describer {
        fn model_name(&self) -> &str {
            "describer"
        }

        async fn chat(
            &self,
            messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, LlmError> {
            self.seen.lock().unwrap().extend(messages);
            let answer = self
                .answer
                .map_err(|()| LlmError::ApiConnection("connection reset".to_string()))?;
            Ok(StepResult {
                message: Message::assistant_text(answer),
                usage: lm_bridge::usage::TokenUsage::empty(),
                finish_reason: Some(lm_bridge::provider::FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    fn describer(answer: Result<&'static str, ()>) -> Arc<Describer> {
        Arc::new(Describer {
            answer,
            seen: Mutex::new(Vec::new()),
        })
    }

    /// The model gets the image with the request to describe it, and its
    /// answer is the description.
    #[tokio::test]
    async fn the_vision_model_describes_the_image() {
        let llm = describer(Ok("a red square"));
        let fallback = LlmVisionFallback::new(llm.clone());

        let description = fallback
            .describe_image("image/png", "AAAA", None)
            .await
            .unwrap();

        assert_eq!(description, "a red square");
        let seen = llm.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].text_content(), DESCRIBE_PROMPT);
        assert!(seen[0].content.iter().any(|part| matches!(
            part,
            ContentPart::ImageUrl { image_url } if image_url.url == "data:image/png;base64,AAAA"
        )));
    }

    /// A question the reading model asks is what the vision model answers.
    #[tokio::test]
    async fn the_vision_model_answers_the_question_asked() {
        let llm = describer(Ok("the arrow goes to storefront-php"));
        let fallback = LlmVisionFallback::new(llm.clone());

        fallback
            .describe_image(
                "image/png",
                "AAAA",
                Some("Where does admin-vue's arrow go?"),
            )
            .await
            .unwrap();

        let asked = llm.seen.lock().unwrap()[0].text_content();
        assert!(
            asked.starts_with("Where does admin-vue's arrow go?"),
            "{asked}"
        );
        assert!(asked.contains("exactly as it is written"), "{asked}");
        assert_eq!(vision_request(DESCRIBE_PROMPT, Some("  ")), DESCRIBE_PROMPT);
    }

    /// No description, or a failed request, is an error the read reports.
    #[tokio::test]
    async fn no_description_is_an_error() {
        let empty = LlmVisionFallback::new(describer(Ok("  ")));
        let err = empty
            .describe_image("image/png", "AAAA", None)
            .await
            .unwrap_err();
        assert!(err.contains("returned no description"), "{err}");

        let failing = LlmVisionFallback::new(describer(Err(())));
        let err = failing
            .describe_image("image/png", "AAAA", None)
            .await
            .unwrap_err();
        assert!(
            err.starts_with("describer could not describe the image"),
            "{err}"
        );
    }
}
