//! Embedded llama.cpp backend — development plan §6.2 (R2), milestone M1.5.
//!
//! Compiled only behind the `embedded` cargo feature (`cargo build -p
//! rusta-llm --features embedded`; needs cmake + a C++ toolchain — the default
//! artifact stays cmake-free). Every llama.cpp call happens on one dedicated
//! OS thread: the C API blocks, so inference never touches the async runtime
//! (plan §6.2). Requests queue through `std::sync::mpsc`, tokens stream back
//! over `tokio::sync::mpsc`, and the single inference thread serializes
//! completions by construction (plan §6.8: one loaded model, one inference
//! thread).
//!
//! Sampling follows plan §6.2: a `LlamaSampler` chain of `temp` (the request's
//! temperature, §7 default 0.2) → `top_p` (config, default 0.9) → greedy. The
//! terminal greedy stage makes completions deterministic, which suits code
//! edits; the chain keeps the documented shape so a stochastic tail (`dist`)
//! can be swapped in deliberately later.
//!
//! Token accounting is exact: [`EmbeddedBackend::count_tokens`] uses the
//! loaded model's tokenizer and [`EmbeddedBackend::context_window`] the GGUF's
//! trained context — no `ceil(chars / 3)` estimation on this backend.

use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;

use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use tokio::sync::mpsc as async_mpsc;

use crate::error::Error;
use crate::types::{ChatRequest, FinishReason, Message, StreamEvent};

/// Prompt-decode granularity — tokens per `llama_decode` call. Must stay ≤ the
/// context's `n_batch`, which is configured to this same value.
const DECODE_CHUNK: usize = 512;

/// Bounded backlog of undelivered stream events.
const EVENT_CHANNEL_CAPACITY: usize = 128;

/// Embedded backend configuration — development plan §7 `[backend.embedded]`.
#[derive(Debug, Clone)]
pub struct EmbeddedConfig {
    /// Path to the GGUF model file.
    pub model_path: PathBuf,
    /// Context window in tokens; `None` reads the model's trained context from
    /// GGUF metadata (§7: "embedded reads GGUF metadata").
    pub ctx_size: Option<u32>,
    /// Layers offloaded to the GPU; `0` = CPU only. Only takes effect when
    /// llama.cpp was built with a GPU backend (`embedded-cuda`).
    pub gpu_layers: u32,
    /// Nucleus-sampling cutoff (§6.2 default 0.9).
    pub top_p: f32,
}

impl EmbeddedConfig {
    /// Configuration for `model_path` with the plan defaults: context size
    /// from GGUF metadata, CPU-only, `top_p` 0.9.
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        Self {
            model_path: model_path.into(),
            ctx_size: None,
            gpu_layers: 0,
            top_p: 0.9,
        }
    }
}

/// Effective context window: a configured size clamped into the model's
/// trained range; `None` adopts the trained size wholesale (§6.2).
fn effective_ctx(configured: Option<u32>, trained: u32) -> u32 {
    match configured {
        Some(requested) => requested.clamp(1, trained),
        None => trained,
    }
}

/// The process-wide llama.cpp backend handle.
///
/// `LlamaBackend::init()` succeeds only once per process; the handle is cached
/// so repeated backend construction (and tests) cannot trip
/// `BackendAlreadyInitialized`. A failed init is permanent for the process and
/// is cached as such.
fn global_backend() -> Result<Arc<LlamaBackend>, Error> {
    static BACKEND: OnceLock<Result<Arc<LlamaBackend>, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| match LlamaBackend::init() {
            Ok(backend) => Ok(Arc::new(backend)),
            Err(error) => Err(format!("llama.cpp backend init failed: {error}")),
        })
        .clone()
        .map_err(|cause| Error::Config { cause })
}

/// In-process llama.cpp backend — development plan §6.2, milestone M1.5.
///
/// All heavy state lives on the inference thread; this type is just a handle
/// to it, shared behind an `Arc` by the layers above. Dropping it closes the
/// job channel, which ends the inference thread.
#[derive(Debug)]
pub struct EmbeddedBackend {
    shared: Arc<Shared>,
}

/// State shared by every backend handle.
#[derive(Debug)]
struct Shared {
    model: Arc<LlamaModel>,
    context_window: u32,
    cancel: Arc<AtomicBool>,
    jobs: Mutex<mpsc::Sender<Job>>,
}

/// One queued completion.
struct Job {
    request: ChatRequest,
    events: async_mpsc::Sender<StreamEvent>,
}

/// Everything the inference thread owns; it never leaves that thread.
struct WorkerEnv {
    backend: Arc<LlamaBackend>,
    model: Arc<LlamaModel>,
    template: LlamaChatTemplate,
    context_window: u32,
    top_p: f32,
    cancel: Arc<AtomicBool>,
}

impl EmbeddedBackend {
    /// Loads the GGUF and starts the inference thread (plan §6.2).
    ///
    /// # Errors
    /// [`Error::Config`] when llama.cpp or the inference thread cannot start;
    /// [`Error::ModelLoad`] when the GGUF fails to load — both carry a remedy.
    pub fn new(config: EmbeddedConfig) -> Result<Self, Error> {
        let backend = global_backend()?;
        let model_params = LlamaModelParams::default().with_n_gpu_layers(config.gpu_layers);
        let model = Arc::new(
            LlamaModel::load_from_file(backend.as_ref(), &config.model_path, &model_params)
                .map_err(|error| Error::ModelLoad {
                    path: config.model_path.display().to_string(),
                    cause: error.to_string(),
                })?,
        );
        let trained = model.n_ctx_train().max(1);
        let context_window = effective_ctx(config.ctx_size, trained);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_env = WorkerEnv {
            backend,
            template: model
                .chat_template(None)
                .unwrap_or_else(|_| fallback_template()),
            model: Arc::clone(&model),
            context_window,
            top_p: sanitized_top_p(config.top_p),
            cancel: Arc::clone(&cancel),
        };
        let (sender, receiver) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("rusta-embedded-llm".to_owned())
            .spawn(move || worker_loop(worker_env, receiver))
            .map_err(|error| Error::Config {
                cause: format!("failed to spawn the inference thread: {error}"),
            })?;
        Ok(Self {
            shared: Arc::new(Shared {
                model,
                context_window,
                cancel,
                jobs: Mutex::new(sender),
            }),
        })
    }

    /// Requests that the in-flight generation stop at the next token boundary
    /// (the §6.2 shutdown flag, checked between tokens). Its stream then ends
    /// with [`StreamEvent::Finish`](`FinishReason::Stop`); the flag clears
    /// when the worker picks up its next job.
    pub fn stop(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
    }

    /// The model's exact token count for `text` (§6.2: no estimation when
    /// embedded). Falls back to the §6.2 heuristic only for text that cannot
    /// be tokenized (interior null bytes).
    pub fn count_tokens(&self, text: &str) -> u64 {
        self.shared
            .model
            .str_to_token(text, AddBos::Never)
            .map(|tokens| tokens.len() as u64)
            .unwrap_or_else(|_| crate::tokens::estimate_tokens(text))
    }

    /// The effective context window in tokens, from GGUF metadata/config.
    pub fn context_window(&self) -> u64 {
        u64::from(self.shared.context_window)
    }

    /// Starts a streaming completion; returns the event channel (plan §6.2).
    ///
    /// Queuing failures (worker stopped) surface here; generation failures are
    /// delivered as a final [`StreamEvent::Failed`] — mirroring the HTTP
    /// backend's split between establishment and mid-stream errors.
    pub fn stream(&self, request: ChatRequest) -> Result<async_mpsc::Receiver<StreamEvent>, Error> {
        let (sender, receiver) = async_mpsc::channel(EVENT_CHANNEL_CAPACITY);
        self.submit(Job {
            request,
            events: sender,
        })?;
        Ok(receiver)
    }

    /// Non-streaming completion — summaries, sub-coder wrap-ups (plan §6.2).
    ///
    /// # Errors
    /// [`Error::Inference`] if generation fails mid-stream.
    pub async fn complete(&self, request: ChatRequest) -> Result<String, Error> {
        let mut events = self.stream(request)?;
        let mut text = String::new();
        while let Some(event) = events.recv().await {
            match event {
                StreamEvent::Delta(delta) => text.push_str(&delta),
                StreamEvent::ToolCall { .. } => {}
                StreamEvent::Finish(_) => return Ok(text),
                StreamEvent::Failed(cause) => return Err(Error::Inference { cause }),
            }
        }
        Err(Error::Inference {
            cause: "inference thread ended the stream without Finish".to_owned(),
        })
    }

    /// Queues `job` onto the inference thread.
    fn submit(&self, job: Job) -> Result<(), Error> {
        self.shared
            .jobs
            .lock()
            .map_err(|_| Error::Inference {
                cause: "inference job queue poisoned".to_owned(),
            })?
            .send(job)
            .map_err(|_| Error::Inference {
                cause: "inference thread has stopped; construct a new backend".to_owned(),
            })
    }
}

// ------------------------------------------------------------ inference core

/// Inference-thread main loop: serve jobs one at a time (inference is
/// serialized by construction) until every handle is dropped and the job
/// channel disconnects.
fn worker_loop(env: WorkerEnv, jobs: mpsc::Receiver<Job>) {
    for Job { request, events } in jobs {
        env.cancel.store(false, Ordering::Relaxed);
        if let Err(cause) = run_generation(&env, &request, &events) {
            // Best-effort: the receiver is often gone for the same reason.
            let _ = events.blocking_send(StreamEvent::Failed(cause));
        }
    }
}

/// Runs one completion and always emits a terminal event: `Finish` on success,
/// `Failed` with a remedy on error (plan §6.11).
fn run_generation(
    env: &WorkerEnv,
    request: &ChatRequest,
    events: &async_mpsc::Sender<StreamEvent>,
) -> Result<FinishReason, String> {
    let reason = generate(env, request, events)?;
    let _ = events.blocking_send(StreamEvent::Finish(reason.clone()));
    Ok(reason)
}

/// One full completion: template render → tokenize → budget check → prompt
/// decode (chunked) → token-by-token generation with stop handling.
fn generate(
    env: &WorkerEnv,
    request: &ChatRequest,
    events: &async_mpsc::Sender<StreamEvent>,
) -> Result<FinishReason, String> {
    let prompt = render_prompt(&env.model, &env.template, &request.messages)?;
    // `str_to_token` parses the special tokens the template emitted
    // (llama-cpp-2 calls llama_tokenize with parse_special = true).
    let tokens = env
        .model
        .str_to_token(&prompt, AddBos::Always)
        .map_err(|e| err_str("tokenizing the rendered prompt failed", e))?;
    let window = env.context_window as usize;
    if tokens.len() + request.max_tokens as usize > window {
        return Err(format!(
            "the prompt is {} tokens and max_tokens is {}; together they exceed the \
             {}-token context window. Remedy: shorten the conversation (summarize older \
             turns), lower max_tokens, or raise ctx_size in [backend.embedded]",
            tokens.len(),
            request.max_tokens,
            window
        ));
    }
    let params = LlamaContextParams::default()
        .with_n_ctx(NonZeroU32::new(env.context_window))
        .with_n_batch(DECODE_CHUNK as u32);
    let mut context: LlamaContext<'_> = env
        .model
        .new_context(env.backend.as_ref(), params)
        .map_err(|e| err_str("creating the llama.cpp context failed", e))?;
    let mut sampler = LlamaSampler::chain(
        [
            LlamaSampler::temp(sanitized_temperature(request.temperature)),
            LlamaSampler::top_p(env.top_p, 1),
            LlamaSampler::greedy(),
        ],
        false,
    );
    // Prompt decode in bounded chunks so large prompts never exceed n_batch
    // in a single llama_decode call; logits are only needed on the final
    // prompt token, which the first sample() reads.
    let chunk_count = tokens.len().div_ceil(DECODE_CHUNK);
    let mut position: i32 = 0;
    let mut batch = LlamaBatch::new(DECODE_CHUNK, 1);
    for (chunk_index, chunk) in tokens.chunks(DECODE_CHUNK).enumerate() {
        let last_chunk = chunk_index + 1 == chunk_count;
        for (offset, token) in chunk.iter().enumerate() {
            let needs_logits = last_chunk && offset + 1 == chunk.len();
            batch
                .add(*token, position, &[0], needs_logits)
                .map_err(|e| err_str("queuing prompt tokens failed", e))?;
            position += 1;
        }
        context
            .decode(&mut batch)
            .map_err(|e| err_str("decoding the prompt failed", e))?;
        batch.clear();
    }
    // Generation loop. The first sample reads the prompt's final-token logits
    // (batch position -1 — llama.cpp's get_logits_ith indexes batch
    // positions, and only the prompt's last token requested them); every
    // later sample reads the single-token generation batch (position 0).
    let mut emitter = DeltaEmitter::new(&request.stop);
    let mut utf8 = encoding_rs::UTF_8.new_decoder();
    let mut next_index: i32 = -1;
    let mut produced: u32 = 0;
    let mut finish = FinishReason::Length;
    while produced < request.max_tokens {
        // Cancellation flag — checked between tokens (§6.2).
        if env.cancel.load(Ordering::Relaxed) {
            finish = FinishReason::Stop;
            break;
        }
        let token = sampler.sample(&context, next_index);
        next_index = 0;
        produced += 1;
        if token == env.model.token_eos() || env.model.is_eog_token(token) {
            finish = FinishReason::Stop;
            break;
        }
        let piece = env
            .model
            .token_to_piece(token, &mut utf8, false, None)
            .map_err(|e| err_str("detokenizing a generated token failed", e))?;
        match emitter.push(&piece) {
            // A stop sequence completed: deliver what precedes it, never the
            // stop itself (OpenAI `stop` semantics).
            Some(before_stop) => {
                // Whether or not the consumer is still there, generation ends.
                deliver(events, &before_stop);
                finish = FinishReason::Stop;
                break;
            }
            None => {
                let release = emitter.release();
                if !deliver(events, &release) {
                    // The consumer went away; stop computing.
                    finish = FinishReason::Stop;
                    break;
                }
            }
        }
        batch
            .add(token, position, &[0], true)
            .map_err(|e| err_str("queuing a generated token failed", e))?;
        position += 1;
        context
            .decode(&mut batch)
            .map_err(|e| err_str("decoding a generated token failed", e))?;
        batch.clear();
    }
    deliver(events, &emitter.flush());
    Ok(finish)
}

// ------------------------------------------------------------------ helpers

/// Renders `messages` through the model's chat template into the inference
/// prompt. `add_ass = true` leaves the assistant's opening tag hanging so the
/// model completes in role (llama-cpp-2's documented recommendation).
fn render_prompt(
    model: &LlamaModel,
    template: &LlamaChatTemplate,
    messages: &[Message],
) -> Result<String, String> {
    let chat: Vec<LlamaChatMessage> = messages
        .iter()
        .map(|message| {
            LlamaChatMessage::new(message.role.as_str().to_owned(), message.content.clone())
        })
        .collect::<Result<_, _>>()
        .map_err(|e| err_str("a chat message contained a null byte", e))?;
    model
        .apply_chat_template(template, &chat, true)
        .map_err(|e| err_str("applying the model's chat template failed", e))
}

/// ChatML fallback for GGUFs without template metadata (§6.2) — the same
/// fallback llama.cpp's server makes. `LlamaChatTemplate::new` fails only on
/// interior null bytes, which a static literal cannot contain.
fn fallback_template() -> LlamaChatTemplate {
    LlamaChatTemplate::new("chatml").expect("the static \"chatml\" literal has no null bytes")
}

/// Keeps the temperature inside llama.cpp's valid range (0 is a division by
/// zero in its temp sampler); degenerate values fall back to the §7 default.
fn sanitized_temperature(temperature: f32) -> f32 {
    if temperature.is_finite() && temperature > 0.0 {
        temperature.clamp(1e-4, 5.0)
    } else {
        0.2
    }
}

/// Clamps top_p into (0, 1]; degenerate values fall back to the §6.2 default.
fn sanitized_top_p(top_p: f32) -> f32 {
    if top_p.is_finite() && top_p > 0.0 {
        top_p.clamp(0.01, 1.0)
    } else {
        0.9
    }
}

/// `"{prefix}: {error}"` — keeps every `map_err` closure one line.
fn err_str(prefix: &str, error: impl std::fmt::Display) -> String {
    format!("{prefix}: {error}")
}

/// Sends one delta; `false` means the receiver is gone and generation should
/// stop computing. Empty deltas never hit the channel.
fn deliver(events: &async_mpsc::Sender<StreamEvent>, delta: &str) -> bool {
    delta.is_empty()
        || events
            .blocking_send(StreamEvent::Delta(delta.to_owned()))
            .is_ok()
}

/// Incremental, stop-sequence-aware text emitter (OpenAI `stop` semantics).
///
/// Text is released only once it can no longer be the prefix of a stop
/// sequence: after every push, the longest possible stop overlap
/// (`max_stop_len − 1` characters) is held back. When the accumulated text
/// ends with a stop sequence, the stop sequence itself is cut — never
/// delivered — and [`DeltaEmitter::push`] returns the not-yet-emitted text
/// preceding it.
struct DeltaEmitter {
    stops: Vec<String>,
    holdback_chars: usize,
    text: String,
    emitted_chars: usize,
}

impl DeltaEmitter {
    /// New emitter over `stops`; empty stop strings are ignored (they would
    /// match everything — llama-server treats them the same way).
    fn new(stops: &[String]) -> Self {
        let stops: Vec<String> = stops
            .iter()
            .filter(|stop| !stop.is_empty())
            .cloned()
            .collect();
        let holdback_chars = stops
            .iter()
            .map(|stop| stop.chars().count())
            .max()
            .map_or(0, |longest| longest.saturating_sub(1));
        Self {
            stops,
            holdback_chars,
            text: String::new(),
            emitted_chars: 0,
        }
    }

    /// Appends `piece`. Returns `Some(remainder)` when a stop sequence just
    /// completed: the un-emitted text preceding it, to deliver before ending.
    fn push(&mut self, piece: &str) -> Option<String> {
        self.text.push_str(piece);
        let matched = self
            .stops
            .iter()
            .find(|stop| self.text.ends_with(stop.as_str()))
            .cloned();
        match matched {
            Some(stop) => {
                self.text.truncate(self.text.len() - stop.len());
                Some(self.take_unemitted())
            }
            None => None,
        }
    }

    /// The portion that can no longer be part of a future stop match.
    fn release(&mut self) -> String {
        let total = self.text.chars().count();
        let safe = total.saturating_sub(self.holdback_chars);
        if safe > self.emitted_chars {
            let delta = slice_by_char(&self.text, self.emitted_chars, safe).to_owned();
            self.emitted_chars = safe;
            delta
        } else {
            String::new()
        }
    }

    /// Releases everything (generation ended without a stop match).
    fn flush(&mut self) -> String {
        self.take_unemitted()
    }

    /// Emits and returns all text not yet delivered.
    fn take_unemitted(&mut self) -> String {
        let total = self.text.chars().count();
        let rest = slice_by_char(&self.text, self.emitted_chars, total).to_owned();
        self.emitted_chars = total;
        rest
    }
}

/// `&text[from_char..to_char]` by character index; out-of-range indices clamp
/// to the string's ends, and both bounds always land on char boundaries.
fn slice_by_char(text: &str, from_char: usize, to_char: usize) -> &str {
    let start = text
        .char_indices()
        .nth(from_char)
        .map_or(text.len(), |(byte, _)| byte);
    let end = text
        .char_indices()
        .nth(to_char)
        .map_or(text.len(), |(byte, _)| byte);
    &text[start..end]
}

// --------------------------------------------------------------------- tests

#[cfg(all(test, feature = "embedded"))]
mod tests {
    use super::{
        DeltaEmitter, effective_ctx, err_str, fallback_template, sanitized_temperature,
        sanitized_top_p, slice_by_char,
    };
    use llama_cpp_2::model::LlamaChatMessage;
    use llama_cpp_2::sampling::LlamaSampler;

    #[test]
    fn ctx_defaults_to_trained_and_clamps() {
        assert_eq!(effective_ctx(None, 8192), 8192);
        assert_eq!(effective_ctx(Some(4096), 8192), 4096);
        assert_eq!(effective_ctx(Some(999_999), 8192), 8192);
        assert_eq!(effective_ctx(Some(0), 8192), 1);
    }

    #[test]
    fn fallback_template_is_chatml() {
        assert_eq!(fallback_template().to_str(), Ok("chatml"));
    }

    #[test]
    fn sampler_chain_builds_with_plan_defaults() {
        // temp → top_p → greedy (§6.2); constructing the chain exercises the
        // C API without needing a loaded model.
        let _sampler = LlamaSampler::chain(
            [
                LlamaSampler::temp(sanitized_temperature(0.2)),
                LlamaSampler::top_p(sanitized_top_p(0.9), 1),
                LlamaSampler::greedy(),
            ],
            false,
        );
    }

    #[test]
    fn sanitizers_clamp_degenerate_values() {
        assert_eq!(sanitized_temperature(0.0), 0.2);
        assert_eq!(sanitized_temperature(f32::NAN), 0.2);
        assert_eq!(sanitized_temperature(100.0), 5.0);
        assert_eq!(sanitized_temperature(0.2), 0.2);
        assert_eq!(sanitized_top_p(0.0), 0.9);
        assert_eq!(sanitized_top_p(f32::NAN), 0.9);
        assert_eq!(sanitized_top_p(1.5), 1.0);
        assert_eq!(sanitized_top_p(0.9), 0.9);
    }

    #[test]
    fn chat_message_conversion_rejects_null_bytes() {
        // render_prompt maps Message roles via role.as_str(); the conversion
        // fails loudly (not silently) on interior null bytes.
        assert!(LlamaChatMessage::new("user".to_owned(), "bad\u{0}byte".to_owned()).is_err());
        assert!(LlamaChatMessage::new("user".to_owned(), "ok".to_owned()).is_ok());
    }

    #[test]
    fn emitter_releases_text_immediately_without_stops() {
        let mut emitter = DeltaEmitter::new(&[]);
        assert_eq!(emitter.push("hello "), None);
        assert_eq!(emitter.release(), "hello ");
        assert_eq!(emitter.push("world"), None);
        assert_eq!(emitter.release(), "world");
        assert_eq!(emitter.flush(), "");
    }

    #[test]
    fn emitter_holds_stop_prefix_and_cuts_the_match() {
        let mut emitter = DeltaEmitter::new(&["END".to_owned()]);
        assert_eq!(emitter.push("foo"), None);
        // Holdback is 2 chars (stop length − 1): only "f" is releasable yet.
        assert_eq!(emitter.release(), "f");
        assert_eq!(emitter.push("E"), None);
        assert_eq!(emitter.release(), "o");
        // …until the stop completes and is cut, never delivered; the final
        // un-emitted "o" is handed back so the full "foo" still arrives.
        assert_eq!(emitter.push("ND"), Some("o".to_owned()));
        assert_eq!(emitter.flush(), "");
    }

    #[test]
    fn emitter_delivers_unemitted_text_before_a_stop() {
        let mut emitter = DeltaEmitter::new(&["STOP".to_owned()]);
        // "hello STOP" arrives as one push: the whole stop is cut and only the
        // un-emitted prefix is handed back for delivery.
        assert_eq!(emitter.push("hello STOP"), Some("hello ".to_owned()));
        assert_eq!(emitter.flush(), "");
    }

    #[test]
    fn emitter_ignores_empty_stops_and_handles_multibyte() {
        let mut emitter = DeltaEmitter::new(&[String::new(), "→完了".to_owned()]);
        assert_eq!(emitter.push("结果→"), None);
        assert_eq!(emitter.push("完了"), Some("结果".to_owned()));
        assert_eq!(emitter.flush(), "");
    }

    #[test]
    fn emitter_flush_releases_held_back_tail() {
        let mut emitter = DeltaEmitter::new(&["ZZ".to_owned()]);
        assert_eq!(emitter.push("ab"), None);
        // The holdback is unconditional (1 char here) even when it cannot
        // possibly start a stop; flush releases it once generation ends.
        assert_eq!(emitter.release(), "a");
        assert_eq!(emitter.flush(), "b");
    }

    #[test]
    fn slice_by_char_is_char_safe() {
        assert_eq!(slice_by_char("déjà", 0, 2), "dé");
        assert_eq!(slice_by_char("déjà", 2, 99), "jà");
        assert_eq!(slice_by_char("déjà", 99, 99), "");
    }

    #[test]
    fn err_str_joins_prefix_and_cause() {
        assert_eq!(err_str("boom", 7), "boom: 7");
    }
}
