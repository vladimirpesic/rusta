use std::time::Duration;

use tokio_util::sync::CancellationToken;

use lm_bridge::errors::LlmError;

use super::llm::{Llm, LlmCallbacks, RetryNotice};
use super::types::StepResult;

// Per-step LLM retry budget: ten attempts. The managed endpoints shed load
// with transient 429/overload responses that clear within a few seconds, and
// a small budget would surface those as hard turn failures.
// The backoff is capped at MAX_BACKOFF_SECS, so the added attempts cost bounded
// wall-clock time, not an unbounded stall.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 10;
const INITIAL_BACKOFF_SECS: f64 = 0.5;
// Per-attempt backoff cap. The early ramp (0.5s, 1s, 2s, …) stays well below
// it; it matters for later attempts and high-attempt configs, where a 32s cap
// lets retries ride out multi-minute provider overload instead of giving up
// after a few seconds.
const MAX_BACKOFF_SECS: f64 = 32.0;
// Up to 25% jitter added on top of the exponential base to avoid herd retries.
const JITTER_FACTOR: f64 = 0.25;

/// How persistently a failing model call is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Attempts for one call, the first included. `0` means the default, so a
    /// misconfiguration cannot wedge the loop into giving up before it starts;
    /// `1` disables retrying.
    pub max_attempts: u32,
    /// Retry without limit (`RUSTA_INFINITE_RETRY`), for unattended runs that
    /// should ride out an outage or a spent quota window rather than fail.
    /// Failures that no retry can fix still end the call; see
    /// [`retryable_without_limit`].
    pub infinite: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            infinite: false,
        }
    }
}

impl RetryPolicy {
    fn effective_max_attempts(self) -> u32 {
        if self.max_attempts == 0 {
            DEFAULT_MAX_ATTEMPTS
        } else {
            self.max_attempts
        }
    }
}

/// Call the model, retrying failures the policy says are worth another attempt.
///
/// Before each retry the caller's [`LlmCallbacks::on_retry`] hears about it:
/// whatever the failed attempt streamed is now superseded. The wait between
/// attempts honours a server `Retry-After` and is cut short by `cancel`.
pub async fn chat_with_retry(
    llm: &dyn Llm,
    messages: Vec<lm_bridge::message::Message>,
    tools: Vec<lm_bridge::tool::Tool>,
    callbacks: LlmCallbacks,
    cancel: &CancellationToken,
    policy: RetryPolicy,
) -> Result<StepResult, LlmError> {
    let max_attempts = policy.effective_max_attempts();
    let mut attempt: u32 = 0;

    loop {
        attempt = attempt.saturating_add(1);
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }

        let error = match llm
            .chat(messages.clone(), tools.clone(), callbacks.clone(), cancel)
            .await
        {
            Ok(result) => return Ok(result),
            Err(error) => error,
        };

        let retry = if policy.infinite {
            retryable_without_limit(&error)
        } else {
            attempt < max_attempts
                && !error.is_fatal()
                && !error.is_context_overflow()
                && llm.is_retryable_error(&error)
        };
        if !retry {
            return Err(error);
        }

        // A server `Retry-After` (carried on the error) overrides the computed
        // backoff — the server told us exactly how long to wait.
        let delay = Duration::from_secs_f64(chosen_delay(attempt, error.retry_after()));
        if let Some(on_retry) = &callbacks.on_retry {
            on_retry(&RetryNotice {
                attempt,
                max_attempts: (!policy.infinite).then_some(max_attempts),
                delay,
                error: error.to_string(),
            });
        }
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancel.cancelled() => return Err(LlmError::Cancelled),
        }
    }
}

/// Whether an unlimited-retry run should try `error` again.
///
/// Everything that could clear with time is retried — including spent quota
/// windows, authentication hiccups and overload, which a bounded run gives up
/// on. What stays fatal is what no amount of waiting fixes: a cancelled call, a
/// context overrun (the turn recovers from it by compacting), a malformed
/// history the turn repairs itself, a deterministic request error, a filtered
/// refusal, or a local configuration problem.
#[must_use]
pub fn retryable_without_limit(error: &LlmError) -> bool {
    if error.is_tool_exchange_structure_error() {
        return false;
    }
    match error {
        LlmError::Cancelled
        | LlmError::ApiContextOverflow
        | LlmError::ApiRequestTooLarge(_)
        | LlmError::Config(_)
        | LlmError::SchemaNormalization(_)
        | LlmError::ToolExecution(_)
        | LlmError::Mcp(_)
        | LlmError::Provider(_) => false,
        LlmError::ApiEmptyResponse { filtered, .. } => !filtered,
        LlmError::Request(e) => !e.is_builder() && !e.is_redirect(),
        LlmError::ApiStatus { status, .. } => !matches!(
            *status,
            reqwest::StatusCode::BAD_REQUEST
                | reqwest::StatusCode::NOT_FOUND
                | reqwest::StatusCode::METHOD_NOT_ALLOWED
                | reqwest::StatusCode::PAYLOAD_TOO_LARGE
                | reqwest::StatusCode::UNPROCESSABLE_ENTITY
        ),
        LlmError::ApiBusinessError { .. }
        | LlmError::ApiConnection(_)
        | LlmError::ApiTimeout
        | LlmError::EventSource(_)
        | LlmError::Json(_) => true,
    }
}

/// The delay before the next attempt: a positive server `Retry-After` if one was
/// provided, otherwise the exponential backoff for this attempt.
pub(crate) fn chosen_delay(attempt: u32, retry_after: Option<Duration>) -> f64 {
    match retry_after {
        Some(d) if d > Duration::ZERO => d.as_secs_f64(),
        _ => backoff_secs(attempt),
    }
}

/// Exponential backoff with random jitter.
///
/// Base delay is `0.5 * 2^(attempt - 1)` seconds, capped at [`MAX_BACKOFF_SECS`],
/// plus up to [`JITTER_FACTOR`] (25%) added on top to avoid thundering herds.
fn backoff_secs(attempt: u32) -> f64 {
    let base =
        (INITIAL_BACKOFF_SECS * 2f64.powi(attempt.saturating_sub(1) as i32)).min(MAX_BACKOFF_SECS);
    base + fastrand::f64() * JITTER_FACTOR * base
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An `Llm` that always fails, counting attempts.
    struct AlwaysFails {
        attempts: Arc<AtomicUsize>,
        filtered: bool,
    }

    #[async_trait::async_trait]
    impl Llm for AlwaysFails {
        fn model_name(&self) -> &str {
            "mock"
        }

        async fn chat(
            &self,
            _messages: Vec<lm_bridge::message::Message>,
            _tools: Vec<lm_bridge::tool::Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, LlmError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(LlmError::empty_response(self.filtered))
        }
    }

    async fn attempts_with_budget(filtered: bool, budget: u32) -> usize {
        let attempts = Arc::new(AtomicUsize::new(0));
        let llm = AlwaysFails {
            attempts: attempts.clone(),
            filtered,
        };
        let _ = chat_with_retry(
            &llm,
            Vec::new(),
            Vec::new(),
            LlmCallbacks::default(),
            &CancellationToken::new(),
            RetryPolicy {
                max_attempts: budget,
                infinite: false,
            },
        )
        .await
        .expect_err("every attempt fails");
        attempts.load(Ordering::SeqCst)
    }

    /// Fails `failures` times with `error`, then succeeds.
    struct FailsThenSucceeds {
        attempts: Arc<AtomicUsize>,
        failures: usize,
        error: fn() -> LlmError,
    }

    #[async_trait::async_trait]
    impl Llm for FailsThenSucceeds {
        fn model_name(&self) -> &str {
            "mock"
        }

        async fn chat(
            &self,
            _messages: Vec<lm_bridge::message::Message>,
            _tools: Vec<lm_bridge::tool::Tool>,
            _callbacks: LlmCallbacks,
            _cancel: &CancellationToken,
        ) -> Result<StepResult, LlmError> {
            let seen = self.attempts.fetch_add(1, Ordering::SeqCst);
            if seen < self.failures {
                return Err((self.error)());
            }
            Ok(StepResult {
                message: lm_bridge::message::Message::assistant_text("done"),
                usage: lm_bridge::TokenUsage::empty(),
                finish_reason: Some(lm_bridge::provider::FinishReason::Stop),
                response_id: None,
                stop_reason: None,
            })
        }
    }

    /// A stream interrupted mid-way is retried, and the caller hears about each
    /// retry so it can mark the partial output as discarded.
    #[tokio::test(start_paused = true)]
    async fn an_interrupted_stream_is_retried_and_announced() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let llm = FailsThenSucceeds {
            attempts: attempts.clone(),
            failures: 2,
            error: || LlmError::ApiConnection("the response stream was interrupted".into()),
        };
        let notices = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = notices.clone();
        let callbacks = LlmCallbacks {
            on_retry: Some(Arc::new(move |notice: &RetryNotice| {
                seen.lock().unwrap().push(notice.clone());
            })),
            ..Default::default()
        };
        let result = chat_with_retry(
            &llm,
            Vec::new(),
            Vec::new(),
            callbacks,
            &CancellationToken::new(),
            RetryPolicy::default(),
        )
        .await
        .expect("the third attempt succeeds");
        assert_eq!(result.text(), "done");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        let notices = notices.lock().unwrap();
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[0].attempt, 1);
        assert_eq!(notices[0].max_attempts, Some(DEFAULT_MAX_ATTEMPTS));
        assert!(notices[0].describe().contains("interrupted"));
    }

    /// Unlimited retry rides out failures a bounded run gives up on — here a
    /// spent quota, twelve times over.
    #[tokio::test(start_paused = true)]
    async fn infinite_retry_outlasts_the_budget_and_fatal_quota_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let llm = FailsThenSucceeds {
            attempts: attempts.clone(),
            failures: 12,
            error: || LlmError::ApiBusinessError {
                code: "1308".into(),
                message: "Usage limit reached".into(),
            },
        };
        let policy = RetryPolicy {
            max_attempts: 3,
            infinite: true,
        };
        let result = chat_with_retry(
            &llm,
            Vec::new(),
            Vec::new(),
            LlmCallbacks::default(),
            &CancellationToken::new(),
            policy,
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(attempts.load(Ordering::SeqCst), 13);
    }

    /// ...but never on what waiting cannot fix.
    #[test]
    fn infinite_retry_still_stops_on_unfixable_errors() {
        assert!(!retryable_without_limit(&LlmError::ApiContextOverflow));
        assert!(!retryable_without_limit(&LlmError::Cancelled));
        assert!(!retryable_without_limit(&LlmError::empty_response(true)));
        assert!(!retryable_without_limit(&LlmError::Config("bad".into())));
        assert!(!retryable_without_limit(&LlmError::ApiStatus {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "invalid request".into(),
            retry_hint: false,
            retry_after: None,
        }));
        assert!(retryable_without_limit(&LlmError::ApiStatus {
            status: reqwest::StatusCode::UNAUTHORIZED,
            message: "token expired".into(),
            retry_hint: false,
            retry_after: None,
        }));
    }

    /// Cancelling during the backoff wait ends the call immediately.
    #[tokio::test]
    async fn cancelling_during_the_backoff_ends_the_call() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let llm = FailsThenSucceeds {
            attempts: attempts.clone(),
            failures: usize::MAX,
            error: || LlmError::ApiStatus {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                message: "slow down".into(),
                retry_hint: false,
                retry_after: Some(Duration::from_secs(3600)),
            },
        };
        let cancel = CancellationToken::new();
        let canceller = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            canceller.cancel();
        });
        let err = chat_with_retry(
            &llm,
            Vec::new(),
            Vec::new(),
            LlmCallbacks::default(),
            &cancel,
            RetryPolicy::default(),
        )
        .await
        .expect_err("cancelled");
        assert!(matches!(err, LlmError::Cancelled));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    /// A provider safety filter is deterministic — the identical request
    /// re-triggers the identical refusal — so it must be attempted exactly once
    /// rather than burning the whole backoff ramp.
    ///
    /// `start_paused` auto-advances the backoff sleeps, so the unfiltered case
    /// below does not spend its real two minutes of ramp.
    #[tokio::test(start_paused = true)]
    async fn a_filtered_empty_response_is_attempted_once() {
        assert_eq!(attempts_with_budget(true, DEFAULT_MAX_ATTEMPTS).await, 1);
    }

    /// An unexplained empty stream gets the full budget.
    #[tokio::test(start_paused = true)]
    async fn an_unexplained_empty_response_uses_the_whole_budget() {
        assert_eq!(
            attempts_with_budget(false, DEFAULT_MAX_ATTEMPTS).await,
            DEFAULT_MAX_ATTEMPTS as usize
        );
    }

    /// The budget is configurable, for a harness that needs the knob to move per
    /// run — `RUSTA_MAX_ATTEMPTS_PER_STEP`.
    #[tokio::test(start_paused = true)]
    async fn the_retry_budget_is_honoured() {
        assert_eq!(attempts_with_budget(false, 3).await, 3);
        assert_eq!(
            attempts_with_budget(false, 1).await,
            1,
            "a budget of 1 disables retrying"
        );
        assert_eq!(
            attempts_with_budget(false, 0).await,
            DEFAULT_MAX_ATTEMPTS as usize,
            "zero means unset, not zero attempts"
        );
    }

    #[test]
    fn test_backoff_increases_and_is_capped() {
        let first = backoff_secs(1);
        let second = backoff_secs(2);
        let third = backoff_secs(3);
        let tenth = backoff_secs(10);

        assert!(first > 0.0);
        assert!(second > first);
        assert!(third > second);
        // The base is capped at MAX_BACKOFF_SECS; jitter adds at most JITTER_FACTOR.
        assert!(tenth <= MAX_BACKOFF_SECS * (1.0 + JITTER_FACTOR));
    }

    #[test]
    fn test_backoff_has_jitter() {
        let first = backoff_secs(1);
        let second = backoff_secs(2);
        // The base is 0.5s and 1.0s; jitter adds up to 25% on top.
        assert!((0.5..=0.5 * (1.0 + JITTER_FACTOR)).contains(&first));
        assert!((1.0..=1.0 * (1.0 + JITTER_FACTOR)).contains(&second));
    }

    #[test]
    fn retry_after_overrides_computed_backoff() {
        // A positive Retry-After is used verbatim, regardless of attempt.
        assert_eq!(chosen_delay(1, Some(Duration::from_secs(7))), 7.0);
        assert_eq!(chosen_delay(5, Some(Duration::from_millis(2500))), 2.5);
    }

    #[test]
    fn absent_or_zero_retry_after_falls_back_to_backoff() {
        // No header: within the attempt-1 backoff window.
        let d = chosen_delay(1, None);
        assert!((0.5..=0.5 * (1.0 + JITTER_FACTOR)).contains(&d));
        // A zero Retry-After is ignored in favor of computed backoff.
        let d = chosen_delay(1, Some(Duration::ZERO));
        assert!((0.5..=0.5 * (1.0 + JITTER_FACTOR)).contains(&d));
    }
}
