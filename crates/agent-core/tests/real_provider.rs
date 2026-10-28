//! Live-provider integration tests.
//!
//! These tests hit real Kimi (Moonshot), Z.AI (GLM) and DeepSeek endpoints
//! using the credentials in the repository `.env` file. They are `#[ignore]` so
//! they do not run during normal `cargo test`; execute them explicitly with:
//!
//! ```bash
//! set -a && source .env && set +a
//! cargo test -p agent-core --test real_provider -- --ignored
//! ```

use std::{
    collections::HashMap,
    env,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use agent_core::{Agent, PermissionMode, ProviderLlm};
use lm_bridge::provider::ThinkingConfig;
use lm_bridge::providers::{DeepseekChatProvider, GlmChatProvider, KimiChatProvider};
use tokio_util::sync::CancellationToken;

/// `key` from the process environment, or else from the repository `.env`
/// file, so the tests run without exporting it first.
///
/// The file is read, never exported: the tests run on parallel threads, and
/// changing the environment while another thread reads it is undefined
/// behaviour.
fn var(key: &str) -> Option<String> {
    static DOTENV: LazyLock<HashMap<String, String>> = LazyLock::new(read_dotenv);
    env::var(key).ok().or_else(|| DOTENV.get(key).cloned())
}

/// The repository `.env` file's `KEY=value` lines, quotes stripped; nothing
/// when there is no file.
fn read_dotenv() -> HashMap<String, String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(".env"))
        .unwrap_or_default();
    let content = std::fs::read_to_string(path).unwrap_or_default();
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            (key.trim().to_string(), value.to_string())
        })
        .collect()
}

fn kimi_llm() -> ProviderLlm {
    let api_key = var("KIMI_API_KEY").expect("KIMI_API_KEY not set");
    // Rusta's own default: a Coding Plan key on its default host and model.
    let base_url =
        var("KIMI_BASE_URL").unwrap_or_else(|| "https://api.kimi.com/coding/v1".to_string());
    let model = var("KIMI_MODEL_NAME").unwrap_or_else(|| "kimi-for-coding".to_string());
    let provider = Box::new(
        KimiChatProvider::new(
            &model,
            Some(api_key),
            base_url,
            lm_bridge::providers::openai_compat::HttpTimeouts::idle(Duration::from_secs(300)),
        )
        .expect("failed to create Kimi provider"),
    );
    ProviderLlm::new(provider)
}

fn glm_llm() -> ProviderLlm {
    let api_key = var("ZAI_API_KEY").expect("ZAI_API_KEY not set");
    let base_url =
        var("ZAI_BASE_URL").unwrap_or_else(|| GlmChatProvider::DEFAULT_BASE_URL.to_string());
    let model = var("ZAI_MODEL_NAME").unwrap_or_else(|| "glm-5.1".to_string());
    let provider = Box::new(
        GlmChatProvider::new(
            &model,
            Some(api_key),
            base_url,
            lm_bridge::providers::openai_compat::HttpTimeouts::idle(Duration::from_secs(300)),
        )
        .expect("failed to create GLM provider"),
    );
    ProviderLlm::new(provider).with_thinking(ThinkingConfig::Disabled)
}

fn deepseek_llm() -> ProviderLlm {
    let api_key = var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY not set");
    let base_url =
        var("DEEPSEEK_BASE_URL").unwrap_or_else(|| "https://api.deepseek.com".to_string());
    let model = var("DEEPSEEK_MODEL_NAME").unwrap_or_else(|| "deepseek-flash".to_string());
    let provider = Box::new(
        DeepseekChatProvider::new(
            &model,
            Some(api_key),
            base_url,
            lm_bridge::providers::openai_compat::HttpTimeouts::idle(Duration::from_secs(300)),
        )
        .expect("failed to create DeepSeek provider"),
    );
    ProviderLlm::new(provider)
}

async fn run_file_read_write_agent_loop(llm: Arc<ProviderLlm>, provider_name: &str) {
    let _ = tracing_subscriber::fmt::try_init();
    let temp = tempfile::tempdir().unwrap();
    let allowed_tools: std::collections::HashSet<String> = ["read_file", "write_file"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let label = provider_name.to_string();
    let hooks = agent_core::agent_loop::types::LoopHooks {
        before_step: Some(std::sync::Arc::new(move |tools| {
            tools.retain(|t| allowed_tools.contains(&t.name));
        })),
        after_step: Some(std::sync::Arc::new(move |result| {
            println!(
                "[{label}] step finish={:?} tool_calls={:?}",
                result.finish_reason,
                result.tool_calls()
            );
        })),
        ..Default::default()
    };
    let mut agent = Agent::new(
        temp.path().to_path_buf(),
        "You are a helpful coding assistant. Use the provided tools when needed.",
    )
    .with_permission_mode(PermissionMode::BypassPermissions)
    .with_loop_hooks(hooks);

    let prompt = format!(
        "Please do the following in the current directory ({dir}):\n\
         1. Use the write_file tool to create a file named 'greeting.txt' with the exact content 'Hello from {provider_name}!'.\n\
         2. Use the read_file tool to read 'greeting.txt' back.\n\
         3. Report the exact content of the file as your final answer.",
        dir = temp.path().display(),
    );

    let start = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        agent.prompt(llm.as_ref(), &prompt, &CancellationToken::new()),
    )
    .await
    .expect("agent loop timed out")
    .expect("agent loop failed");
    let elapsed = start.elapsed();

    let file_path = temp.path().join("greeting.txt");
    let on_disk = tokio::fs::read_to_string(&file_path)
        .await
        .expect("greeting.txt was not created");
    assert_eq!(on_disk.trim(), format!("Hello from {provider_name}!"));

    println!("[{provider_name}] elapsed: {elapsed:?}");
    println!(
        "[{provider_name}] finish_reason: {:?}",
        result.finish_reason
    );
    println!("[{provider_name}] usage: {:?}", result.usage);

    assert!(
        result.tool_calls().is_empty(),
        "agent should finish after reporting the answer"
    );
}

#[tokio::test]
#[ignore = "hits the live Kimi API"]
async fn test_kimi_agent_loop_file_read_write() {
    let llm = Arc::new(kimi_llm());
    run_file_read_write_agent_loop(llm, "Kimi").await;
}

#[tokio::test]
#[ignore = "hits the live Z.AI GLM API"]
async fn test_glm_agent_loop_file_read_write() {
    let llm = Arc::new(glm_llm());
    run_file_read_write_agent_loop(llm, "GLM").await;
}

#[tokio::test]
#[ignore = "hits the live DeepSeek API"]
async fn test_deepseek_agent_loop_file_read_write() {
    let llm = Arc::new(deepseek_llm());
    run_file_read_write_agent_loop(llm, "DeepSeek").await;
}

#[tokio::test]
#[ignore = "hits the live Kimi API"]
async fn test_kimi_thinking_and_reasoning() {
    let _ = tracing_subscriber::fmt::try_init();
    let llm = Arc::new(
        kimi_llm()
            .with_thinking(ThinkingConfig::Enabled)
            .with_reasoning_effort("high"),
    );
    let mut agent = Agent::new(
        std::path::PathBuf::from("."),
        "You are a helpful assistant. When asked a simple reasoning question, think step by step briefly and then give the final answer.",
    )
    .with_permission_mode(PermissionMode::BypassPermissions);

    let captured = Arc::new(std::sync::Mutex::new(String::new()));
    let captured_text = Arc::clone(&captured);
    let captured_think = Arc::clone(&captured);
    let callbacks = agent_core::agent_loop::llm::LlmCallbacks {
        on_text_delta: Some(Arc::new(move |delta| {
            captured_text.lock().unwrap().push_str(delta);
        })),
        on_think_delta: Some(Arc::new(move |delta| {
            captured_think.lock().unwrap().push_str(delta);
        })),
        ..Default::default()
    };

    let result = tokio::time::timeout(
        Duration::from_secs(120),
        agent.prompt_with_callbacks(
            llm.as_ref(),
            "What is 7 + 5? Show your reasoning, then state the final answer.",
            callbacks,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("agent loop timed out")
    .expect("agent loop failed");

    let final_text = captured.lock().unwrap().clone();

    println!("[Kimi thinking] finish_reason: {:?}", result.finish_reason);
    println!("[Kimi thinking] usage: {:?}", result.usage);
    println!("[Kimi thinking] text: {final_text}");

    assert_eq!(
        result.finish_reason,
        Some(lm_bridge::provider::FinishReason::Stop)
    );
    assert!(
        result.usage.total_tokens > 0,
        "usage should report consumed tokens"
    );
    assert!(final_text.contains("12"), "final answer should contain 12");
}
