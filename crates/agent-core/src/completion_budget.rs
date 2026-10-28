//! Per-request completion-token budget.
//!
//! A cap is sent only when it was explicitly configured; otherwise the
//! provider's own hook decides whether a default goes out.

use lm_bridge::{message::Message, tool::Tool};

const MIN_FLOOR: u64 = 1;

/// A resolved completion-budget configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct CompletionBudget {
    /// Explicit hard cap (from env or config).
    hard_cap: Option<u64>,
    /// Switched off by a variable of 0 or below: no cap goes out at all, not
    /// even the provider's default for an uncapped request. An explicit
    /// `max_tokens` request is still honoured.
    disabled: bool,
}

/// The variables a budget is read from, the alias last.
const BUDGET_VARS: [&str; 2] = ["KIMI_MODEL_MAX_COMPLETION_TOKENS", "KIMI_MODEL_MAX_TOKENS"];

impl CompletionBudget {
    /// Build a budget from the environment variables
    /// `KIMI_MODEL_MAX_COMPLETION_TOKENS` and its alias `KIMI_MODEL_MAX_TOKENS`,
    /// falling back to an explicit config value. A variable of 0 or below
    /// opts out of any cap.
    #[must_use]
    pub fn resolve(max_output_size: Option<u64>) -> Self {
        Self::resolve_with(max_output_size, |name| std::env::var(name).ok())
    }

    /// [`Self::resolve`], reading the variables through `env`.
    #[must_use]
    pub fn resolve_with(
        max_output_size: Option<u64>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Self {
        for name in BUDGET_VARS {
            match parse_env_budget(env(name).as_deref()) {
                Some(EnvBudgetValue::Disabled) => {
                    return Self {
                        hard_cap: None,
                        disabled: true,
                    };
                }
                Some(EnvBudgetValue::Present(n)) => {
                    return Self {
                        hard_cap: Some(n),
                        disabled: false,
                    };
                }
                None => {}
            }
        }
        Self {
            hard_cap: max_output_size.filter(|&s| s > 0),
            disabled: false,
        }
    }

    /// The explicitly configured `max_completion_tokens` cap, if any.
    ///
    /// `input_tokens` is the estimated tokens already consumed by the prompt.
    /// `context_window` is the model's maximum context size; `0` means
    /// unknown. The cap never exceeds the room left in the window.
    #[must_use]
    pub fn cap(&self, input_tokens: u64, context_window: u64) -> Option<u64> {
        let cap = self.hard_cap?;
        let safe = if context_window > 0 {
            context_window.saturating_sub(input_tokens)
        } else {
            cap
        };
        Some(cap.min(safe).max(MIN_FLOOR))
    }
}

#[derive(Debug, Clone, Copy)]
enum EnvBudgetValue {
    Disabled,
    Present(u64),
}

/// A budget variable's value. Unset, exported empty (as sourcing
/// `.env.example` leaves it) or not a number, it says nothing, and the next
/// source is consulted.
fn parse_env_budget(raw: Option<&str>) -> Option<EnvBudgetValue> {
    let n: i64 = raw?.trim().parse().ok()?;
    Some(if n <= 0 {
        EnvBudgetValue::Disabled
    } else {
        EnvBudgetValue::Present(n.unsigned_abs())
    })
}

/// Estimate the number of tokens consumed by a request's prompt.
#[must_use]
pub fn estimate_input_tokens(messages: &[Message], tools: &[Tool]) -> u64 {
    let message_tokens: u64 = messages
        .iter()
        .map(crate::context_compaction::estimate_tokens_for_message)
        .sum();
    let tool_tokens: u64 = tools
        .iter()
        .map(|t| crate::context_compaction::estimate_tokens(&t.name))
        .sum();
    message_tokens + tool_tokens
}

/// Resolve the effective `max_tokens` for a request.
///
/// `requested` is a caller-supplied value (`max_tokens` config,
/// `RUSTA_MAX_TOKENS`); `when_unset` is the provider's default for requests
/// with no explicit cap at all (Kimi and GLM supply one, generic endpoints
/// send nothing), which a disabled budget leaves out. A configured budget
/// bounds the request; an explicit request is still bounded by the room left
/// in a known context window.
#[must_use]
pub fn clamp_max_tokens(
    requested: Option<u32>,
    budget: &CompletionBudget,
    input_tokens: u64,
    context_window: u64,
    when_unset: Option<u32>,
) -> Option<u32> {
    let requested = requested.map(u64::from);
    let cap = budget.cap(input_tokens, context_window);
    let effective = match (requested, cap) {
        (Some(n), Some(cap)) => n.min(cap),
        (Some(n), None) => {
            let safe = if context_window > 0 {
                context_window.saturating_sub(input_tokens)
            } else {
                n
            };
            n.min(safe)
        }
        (None, Some(cap)) => cap,
        (None, None) if budget.disabled => return None,
        (None, None) => return when_unset,
    };
    Some(effective.max(MIN_FLOOR).min(u64::from(u32::MAX)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An environment with no budget variable, whatever the process has.
    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn test_no_cap_configured_means_no_cap() {
        let budget = CompletionBudget::resolve_with(None, no_env);
        // Unknown context window: nothing to clamp against, so no cap.
        assert_eq!(budget.cap(0, 0), None);
        // Known window: still nothing, however much room is left.
        assert_eq!(budget.cap(2_000, 8_192), None);
    }

    #[test]
    fn test_configured_cap_respects_safe_budget() {
        let budget = CompletionBudget::resolve_with(Some(10_000), no_env);
        assert_eq!(budget.cap(0, 0), Some(10_000));
        // 8k window, 7.5k input -> only 500 tokens safe.
        assert_eq!(budget.cap(7_500, 8_000), Some(500));
    }

    #[test]
    fn test_non_positive_config_value_disables_the_cap() {
        let budget = CompletionBudget::resolve_with(Some(0), no_env);
        assert_eq!(budget.cap(0, 0), None);
    }

    #[test]
    fn test_clamp_without_anything_configured_sends_the_provider_default() {
        let budget = CompletionBudget::resolve_with(None, no_env);
        assert_eq!(clamp_max_tokens(None, &budget, 0, 0, None), None);
        assert_eq!(
            clamp_max_tokens(None, &budget, 2_000, 8_192, Some(6_192)),
            Some(6_192)
        );
        assert_eq!(
            clamp_max_tokens(None, &budget, 2_000, 8_192, Some(u32::MAX)),
            Some(u32::MAX)
        );
    }

    #[test]
    fn test_clamp_with_a_cap_ignores_the_provider_default() {
        let budget = CompletionBudget::resolve_with(Some(4_000), no_env);
        assert_eq!(
            clamp_max_tokens(None, &budget, 0, 0, Some(999)),
            Some(4_000)
        );
        assert_eq!(
            clamp_max_tokens(Some(500), &budget, 0, 0, Some(999)),
            Some(500)
        );
    }

    fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            vars.iter()
                .find(|(var, _)| *var == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    /// A variable of 0 or below opts out of any cap: not even Kimi's default
    /// for an uncapped request (the room left in its window) goes out, while
    /// an explicit request is still honoured.
    #[test]
    fn a_non_positive_variable_sends_no_cap() {
        for vars in [
            &[("KIMI_MODEL_MAX_COMPLETION_TOKENS", "0")][..],
            &[("KIMI_MODEL_MAX_COMPLETION_TOKENS", "-1")],
            &[("KIMI_MODEL_MAX_TOKENS", "0")],
        ] {
            let budget = CompletionBudget::resolve_with(None, env(vars));
            let kimi_default = Some(258_144);
            assert_eq!(
                clamp_max_tokens(None, &budget, 4_000, 262_144, kimi_default),
                None,
                "{vars:?}"
            );
            assert_eq!(
                clamp_max_tokens(Some(8_000), &budget, 4_000, 262_144, kimi_default),
                Some(8_000),
                "{vars:?}: an explicit request stands"
            );
        }
    }

    /// The full name wins over the alias; an empty or malformed
    /// one says nothing, so the next source applies.
    #[test]
    fn budget_variables_are_read_in_order() {
        let budget = CompletionBudget::resolve_with(
            None,
            env(&[
                ("KIMI_MODEL_MAX_COMPLETION_TOKENS", "4096"),
                ("KIMI_MODEL_MAX_TOKENS", "0"),
            ]),
        );
        assert_eq!(budget.cap(0, 0), Some(4_096));
        let budget = CompletionBudget::resolve_with(
            Some(9_000),
            env(&[
                ("KIMI_MODEL_MAX_COMPLETION_TOKENS", ""),
                ("KIMI_MODEL_MAX_TOKENS", "lots"),
            ]),
        );
        assert_eq!(budget.cap(0, 0), Some(9_000));
    }

    #[test]
    fn test_clamp_an_explicit_request_to_the_remaining_window() {
        let budget = CompletionBudget::resolve_with(None, no_env);
        // No budget, but an explicit request is still bounded by the window.
        assert_eq!(
            clamp_max_tokens(Some(10_000), &budget, 7_500, 8_000, None),
            Some(500)
        );
        // Unknown window: the request stands as configured.
        assert_eq!(
            clamp_max_tokens(Some(10_000), &budget, 0, 0, None),
            Some(10_000)
        );
    }
}
