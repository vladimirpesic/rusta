//! A title for a session that has none, made after a completed turn in the
//! REPL or the TUI from the session's first exchange (`auto_session_title`,
//! on by default).
//!
//! On the Kimi Coding Plan the plan's own title service writes it. Elsewhere
//! the session's provider is asked once, with thinking off; on DeepSeek,
//! `deepseek-flash` is asked. The title is made in the background and is best
//! effort: a failure leaves the session untitled, to try again after a later
//! turn, at most three times in a run.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use agent_core::agent_loop::llm::{Llm, LlmCallbacks, ProviderLlm};
use lm_bridge::message::Message;
use lm_bridge::provider::ThinkingConfig;
use lm_bridge::providers::openai_compat::HttpTimeouts;
use tokio_util::sync::CancellationToken;

use crate::cli::Cli;
use crate::commands::CommandContext;
use crate::endpoints::Billing;

/// Attempts a run makes at a title.
pub const MAX_ATTEMPTS: u32 = 3;

/// The longest title kept, in characters.
const MAX_TITLE_CHARS: usize = 200;

/// How much of the first prompt and of its answer a title is made from, in
/// characters.
const MAX_PROMPT_CHARS: usize = 400;
const MAX_ANSWER_CHARS: usize = 300;

/// DeepSeek's fast and cheap model, which titles a DeepSeek session.
const DEEPSEEK_TITLE_MODEL: &str = "deepseek-flash";

/// What a model is asked for a title.
const TITLE_PROMPT: &str = "Write a title for the conversation below, between a user and a \
     coding agent. Name its task in at most eight words, in the conversation's language. \
     Answer with the title alone, without quotes or a closing period.";

/// The session's title in this run, and the attempts at making one.
#[derive(Debug, Default)]
pub struct SessionTitle {
    current: Mutex<String>,
    attempts: AtomicU32,
    in_flight: AtomicBool,
}

impl SessionTitle {
    /// The title a session starts the run with: empty when it has none.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            current: Mutex::new(title.into()),
            ..Self::default()
        }
    }

    /// The title, empty while the session has none.
    #[must_use]
    pub fn current(&self) -> String {
        self.current.lock().unwrap().clone()
    }

    /// The session's title from now on.
    pub fn set(&self, title: impl Into<String>) {
        *self.current.lock().unwrap() = title.into();
    }

    /// Start an attempt at a title: `false` while one is under way, or once
    /// [`MAX_ATTEMPTS`] have been made.
    fn begin_attempt(&self) -> bool {
        if self.in_flight.swap(true, Ordering::SeqCst) {
            return false;
        }
        // Holding `in_flight`, nothing else counts attempts.
        let made = self.attempts.load(Ordering::SeqCst);
        if made >= MAX_ATTEMPTS {
            self.in_flight.store(false, Ordering::SeqCst);
            return false;
        }
        self.attempts.store(made + 1, Ordering::SeqCst);
        true
    }

    fn end_attempt(&self) {
        self.in_flight.store(false, Ordering::SeqCst);
    }
}

/// After a completed turn: start making a title for a session that has
/// none, in the background, from `first_exchange`
/// ([`agent_core::Agent::first_exchange`]). The caller takes it from the
/// agent it holds: the REPL still holds it here, so locking it again would
/// never return.
pub fn after_turn(ctx: &CommandContext, first_exchange: Option<(String, String)>) {
    if ctx.config.auto_session_title == Some(false)
        || !ctx.session_title.current().trim().is_empty()
    {
        return;
    }
    let Some((prompt, answer)) = first_exchange else {
        return;
    };
    let state = Arc::clone(&ctx.session_title);
    if !state.begin_attempt() {
        return;
    }
    let cli = ctx.cli.clone();
    let timeouts = ctx.timeouts;
    let store = Arc::clone(&ctx.session_store);
    let agent = Arc::clone(&ctx.agent);
    tokio::spawn(async move {
        let id = crate::session_id(&cli);
        match make(&cli, timeouts, &chat_content(&prompt, &answer)).await {
            Ok(title) => {
                // The store is let go before the agent is taken: a turn
                // holds the agent and, once over, takes the store.
                let saved = store.lock().await.title_if_untitled(&id, &title).await;
                match saved {
                    Ok(true) => {
                        state.set(title.clone());
                        agent.lock().await.set_session_title(title);
                    }
                    // Titled with `/title` in the meantime.
                    Ok(false) => {}
                    Err(e) => tracing::warn!("could not save the session title: {e}"),
                }
            }
            Err(e) => tracing::debug!("no session title this time: {e}"),
        }
        state.end_attempt();
    });
}

/// The first exchange as a title is made from: `user: …` and `assistant: …`
/// lines, each cut short.
fn chat_content(prompt: &str, answer: &str) -> String {
    let cut = |text: &str, chars: usize| text.trim().chars().take(chars).collect::<String>();
    format!(
        "user: {}\nassistant: {}",
        cut(prompt, MAX_PROMPT_CHARS),
        cut(answer, MAX_ANSWER_CHARS)
    )
}

/// A title for `chat`, by the Kimi Coding Plan's title service or by the
/// session's provider.
async fn make(cli: &Cli, timeouts: HttpTimeouts, chat: &str) -> Result<String, String> {
    let provider = cli.provider.clone().unwrap_or_else(|| "kimi".to_string());
    let answer = if provider == "kimi" && cli.billing.unwrap_or_default() == Billing::Subscription {
        let services = crate::native_web::kimi_services(cli).ok_or("no Kimi API key")?;
        services.chat_title(chat).await.map_err(|e| e.to_string())?
    } else {
        let model = if provider == "deepseek" {
            Some(DEEPSEEK_TITLE_MODEL.to_string())
        } else {
            cli.model.clone()
        };
        let provider = crate::build_provider_for(cli, Some(provider), model, timeouts)
            .map_err(|e| e.to_string())?;
        let llm = ProviderLlm::new(provider).with_thinking(ThinkingConfig::Disabled);
        ask(&llm, chat).await?
    };
    clean(&answer).ok_or_else(|| "the title came back empty".to_string())
}

/// Ask `llm` for a title for `chat`.
async fn ask(llm: &dyn Llm, chat: &str) -> Result<String, String> {
    let step = llm
        .chat(
            vec![Message::system(TITLE_PROMPT), Message::user(chat)],
            Vec::new(),
            LlmCallbacks::default(),
            &CancellationToken::new(),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(step.message.text_content())
}

/// A model's answer as a title: its first line with text, without a
/// `Title:` label, Markdown heading or emphasis marks, wrapping quotes or a
/// closing period, and within [`MAX_TITLE_CHARS`]. `None` when nothing is
/// left.
fn clean(answer: &str) -> Option<String> {
    let line = answer
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let line = line.trim_start_matches('#').trim_start();
    let line = ["Title:", "title:", "TITLE:"]
        .iter()
        .find_map(|label| line.strip_prefix(label))
        .unwrap_or(line);
    let title = line
        .trim()
        .trim_matches(|c| matches!(c, '"' | '\'' | '“' | '”' | '‘' | '’' | '`' | '*' | '_'))
        .trim()
        .trim_end_matches('.')
        .trim();
    let title: String = title.chars().take(MAX_TITLE_CHARS).collect();
    let title = title.trim_end().to_string();
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    use agent_core::agent_loop::types::StepResult;
    use lm_bridge::errors::LlmError;
    use lm_bridge::provider::FinishReason;
    use lm_bridge::tool::Tool;
    use lm_bridge::usage::TokenUsage;

    use super::*;

    /// A model that answers with a fixed text and keeps what it was sent.
    struct Titler {
        answer: String,
        sent: Mutex<Vec<Message>>,
    }

    #[async_trait::async_trait]
    impl Llm for Titler {
        fn model_name(&self) -> &str {
            "titler"
        }

        async fn chat(
            &self,
            messages: Vec<Message>,
            _tools: Vec<Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, LlmError> {
            *self.sent.lock().unwrap() = messages;
            Ok(StepResult {
                message: Message::assistant_text(self.answer.clone()),
                usage: TokenUsage::empty(),
                finish_reason: Some(FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    /// The model is asked with the title prompt and the first exchange.
    #[tokio::test]
    async fn the_model_is_asked_for_a_title_for_the_first_exchange() {
        let titler = Titler {
            answer: "Fix the login test".to_string(),
            sent: Mutex::new(Vec::new()),
        };
        let chat = chat_content("Fix the login test.", "Fixed.");
        assert_eq!(ask(&titler, &chat).await.unwrap(), "Fix the login test");
        let sent = titler.sent.lock().unwrap();
        assert_eq!(sent[0].text_content(), TITLE_PROMPT);
        assert_eq!(
            sent[1].text_content(),
            "user: Fix the login test.\nassistant: Fixed."
        );
    }

    /// The exchange is cut to what a title is made from.
    #[test]
    fn the_exchange_is_cut_short() {
        let chat = chat_content(&"p".repeat(500), &format!("  {}  ", "a".repeat(500)));
        let (user, assistant) = chat.split_once('\n').unwrap();
        assert_eq!(user, format!("user: {}", "p".repeat(MAX_PROMPT_CHARS)));
        assert_eq!(
            assistant,
            format!("assistant: {}", "a".repeat(MAX_ANSWER_CHARS))
        );
    }

    /// What a model wraps a title in is taken off.
    #[test]
    fn a_title_is_the_answer_without_its_wrapping() {
        for answer in [
            "Fix the login test",
            "\n  \"Fix the login test.\"\n",
            "Title: Fix the login test",
            "# **Fix the login test**",
            "“Fix the login test”\nThe user asked to fix a test.",
        ] {
            assert_eq!(
                clean(answer).as_deref(),
                Some("Fix the login test"),
                "{answer:?}"
            );
        }
        assert_eq!(clean("  \n\"\"\n"), None);
        assert_eq!(clean(""), None);
        let long = clean(&"word ".repeat(100)).unwrap();
        assert_eq!(
            long.chars().count(),
            MAX_TITLE_CHARS - 1,
            "cut, then trimmed"
        );
    }

    /// One attempt at a time, and no more than the cap in a run.
    #[test]
    fn attempts_are_one_at_a_time_and_capped() {
        let state = SessionTitle::new("");
        assert!(state.begin_attempt());
        assert!(!state.begin_attempt(), "one is under way");
        state.end_attempt();
        assert!(state.begin_attempt());
        state.end_attempt();
        assert!(state.begin_attempt());
        state.end_attempt();
        assert!(!state.begin_attempt(), "three were made");
        assert!(
            !state.in_flight.load(Ordering::SeqCst),
            "a refused attempt leaves none under way"
        );
    }

    /// A session with a title, or with titles turned off, gets none made;
    /// nor does one whose first turn has no answer.
    #[tokio::test]
    async fn no_title_is_made_for_a_titled_session_or_when_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = CommandContext::for_test(dir.path()).await;
        after_turn(&ctx, None);
        assert_eq!(
            ctx.session_title.attempts.load(Ordering::SeqCst),
            0,
            "no exchange yet"
        );

        ctx.agent
            .lock()
            .await
            .prompt(
                &Titler {
                    answer: "Done.".to_string(),
                    sent: Mutex::new(Vec::new()),
                },
                "Fix the login test.",
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        let exchange = ctx.agent.lock().await.first_exchange();
        assert!(exchange.is_some());
        ctx.config.auto_session_title = Some(false);
        after_turn(&ctx, exchange.clone());
        assert_eq!(ctx.session_title.attempts.load(Ordering::SeqCst), 0);

        ctx.config.auto_session_title = None;
        ctx.session_title.set("Login test");
        after_turn(&ctx, exchange.clone());
        assert_eq!(ctx.session_title.attempts.load(Ordering::SeqCst), 0);

        // The attempt asks a closed local port, never a vendor.
        ctx.cli = deepseek_cli("http://127.0.0.1:9".to_string());
        ctx.session_title.set("");
        after_turn(&ctx, exchange);
        assert_eq!(ctx.session_title.attempts.load(Ordering::SeqCst), 1);
    }

    /// A title made after the first turn reaches the session index, the
    /// run's title and the agent (so the hooks); the session is not titled
    /// again.
    #[tokio::test]
    async fn a_title_made_after_the_first_turn_is_kept() {
        let server = chat_server("Fix the login test").await;
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = CommandContext::for_test(dir.path()).await;
        ctx.cli = deepseek_cli(server.uri());
        ctx.agent
            .lock()
            .await
            .prompt(
                &Titler {
                    answer: "Fixed: the cookie was renamed.".to_string(),
                    sent: Mutex::new(Vec::new()),
                },
                "Fix the login test.",
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        crate::record_session_activity(&ctx, None).await;

        {
            // As the REPL calls it: with the agent still held.
            let agent = ctx.agent.lock().await;
            after_turn(&ctx, agent.first_exchange());
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while ctx.session_title.in_flight.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the title is made");

        assert_eq!(ctx.session_title.current(), "Fix the login test");
        let entry = ctx.session_store.lock().await.get("default").unwrap();
        assert_eq!(entry.label(), "Fix the login test");
        assert_eq!(ctx.agent.lock().await.session_title(), "Fix the login test");
        assert_eq!(
            request(&server).await["messages"][1]["content"],
            "user: Fix the login test.\nassistant: Fixed: the cookie was renamed."
        );
        let exchange = ctx.agent.lock().await.first_exchange();
        after_turn(&ctx, exchange);
        assert_eq!(ctx.session_title.attempts.load(Ordering::SeqCst), 1);
    }

    /// A DeepSeek session on V4-Pro.
    fn deepseek_cli(base_url: String) -> Cli {
        Cli {
            provider: Some("deepseek".to_string()),
            model: Some("deepseek-v4-pro".to_string()),
            api_key: Some("sk-test".to_string()),
            base_url: Some(base_url),
            ..Cli::default()
        }
    }

    fn timeouts() -> HttpTimeouts {
        HttpTimeouts::idle(std::time::Duration::from_secs(5))
    }

    /// A chat endpoint that streams `answer`.
    async fn chat_server(answer: &str) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
            serde_json::json!({
                "id": "chatcmpl-test",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "test",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
        };
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            chunk(
                serde_json::json!({"role": "assistant", "content": answer}),
                serde_json::Value::Null
            ),
            chunk(serde_json::json!({}), serde_json::json!("stop")),
        );
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .mount(&server)
            .await;
        server
    }

    /// The request a chat server got.
    async fn request(server: &wiremock::MockServer) -> serde_json::Value {
        let requests = server.received_requests().await.unwrap();
        serde_json::from_slice(&requests[0].body).unwrap()
    }

    /// On the Kimi Coding Plan, the plan's title service writes the title.
    #[tokio::test]
    async fn the_kimi_coding_plan_titles_with_its_title_service() {
        use wiremock::matchers::{body_partial_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tools"))
            .and(body_partial_json(serde_json::json!({
                "method": "chat_title",
                "params": { "chat_content": "user: Fix it.\nassistant: Fixed." },
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "title": "Fix login test." })),
            )
            .mount(&server)
            .await;
        let cli = Cli {
            provider: Some("kimi".to_string()),
            billing: Some(Billing::Subscription),
            api_key: Some("sk-test".to_string()),
            base_url: Some(server.uri()),
            ..Cli::default()
        };
        let title = make(&cli, timeouts(), "user: Fix it.\nassistant: Fixed.")
            .await
            .unwrap();
        assert_eq!(title, "Fix login test");
    }

    /// On DeepSeek, `deepseek-flash` writes the title, thinking off, whatever
    /// model the session runs.
    #[tokio::test]
    async fn deepseek_titles_with_its_flash_model() {
        let server = chat_server("\"Fix the login test.\"").await;
        let title = make(
            &deepseek_cli(server.uri()),
            timeouts(),
            "user: x\nassistant: y",
        )
        .await
        .unwrap();
        assert_eq!(title, "Fix the login test");
        let request = request(&server).await;
        assert_eq!(request["model"], DEEPSEEK_TITLE_MODEL);
        assert_eq!(request["thinking"], serde_json::json!({"type": "disabled"}));
        assert_eq!(request["messages"][0]["content"], TITLE_PROMPT);
        assert!(request.get("tools").is_none(), "{request}");
    }

    /// Elsewhere the session's own model writes it, thinking off: on
    /// GLM-5.3, which cannot turn thinking off, its lightest thinking.
    #[tokio::test]
    async fn another_provider_titles_with_the_sessions_model() {
        let server = chat_server("Port the lexer").await;
        let cli = Cli {
            provider: Some("glm".to_string()),
            model: Some("glm-5.3".to_string()),
            api_key: Some("sk-test".to_string()),
            base_url: Some(server.uri()),
            ..Cli::default()
        };
        let title = make(&cli, timeouts(), "user: x\nassistant: y")
            .await
            .unwrap();
        assert_eq!(title, "Port the lexer");
        let request = request(&server).await;
        assert_eq!(request["model"], "glm-5.3");
        assert_eq!(request["reasoning_effort"], "low", "{request}");
    }
}
