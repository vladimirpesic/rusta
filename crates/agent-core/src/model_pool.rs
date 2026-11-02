//! A declarative pool of models sub-agents may be spawned on.
//!
//! Without this, every sub-agent inherits the parent's model. That is the wrong
//! default on cost: a throwaway "find where this symbol is defined" delegation
//! runs on the flagship at `max` reasoning effort, when a small fast model would
//! answer it as well and for a fraction of the tokens. Routing that class of
//! work elsewhere while the parent stays on the flagship is the single largest
//! cost lever a harness has.
//!
//! The pool is *declarative*: the operator names aliases and describes what each
//! is for, and those descriptions are rendered into the `agent` and
//! `agent_swarm` tool descriptions so the model picks by purpose rather than by
//! model id. Rusta never guesses — with no pool configured, the `model`
//! parameter is not advertised at all and every sub-agent inherits the
//! parent.

use std::sync::Arc;

use crate::agent_loop::llm::Llm;

/// The alias meaning "whatever the parent is running on".
///
/// Reserved: a pool entry may not claim it, because the caller's own model is
/// not the pool's to redefine, and a pool that shadowed it would silently
/// redirect every sub-agent that asked to stay put.
pub const PRIMARY_ALIAS: &str = "primary";

/// One selectable entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPoolEntry {
    /// The name the model uses to select this entry.
    pub alias: String,
    /// The model id it resolves to. Shown to the model so it can tell two
    /// aliases apart when the descriptions are similar.
    pub model: String,
    /// What this entry is *for*. This is the part the model actually chooses
    /// on, so it should describe the work, not the model's specifications.
    pub description: String,
}

/// A pool of models sub-agents may be bound to.
///
/// Implemented outside `agent-core`, because building a provider means knowing
/// about endpoints, billing plans and API keys — all of which belong to the CLI.
pub trait ModelPool: Send + Sync {
    /// The selectable entries, in the order they should be advertised.
    fn entries(&self) -> &[ModelPoolEntry];

    /// The alias used when the caller names none. `None` means "inherit the
    /// parent", which is also what an empty pool implies.
    fn default_alias(&self) -> Option<&str>;

    /// Build an LLM for `alias`.
    ///
    /// Never called with [`PRIMARY_ALIAS`]; that is resolved to the parent's own
    /// LLM before the pool is consulted.
    fn resolve(&self, alias: &str) -> Result<Arc<dyn Llm>, String>;
}

/// Which model a sub-agent should run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// Inherit the parent's LLM.
    Primary,
    /// Bind to a pool entry.
    Alias(String),
}

/// Resolve the caller's `model` argument against `pool`.
///
/// The rules, in order:
///
/// * No pool → always [`Binding::Primary`]. The parameter is not advertised in
///   that case, so a value here came from a stale schema and is ignored rather
///   than failing the call.
/// * An explicit [`PRIMARY_ALIAS`] → [`Binding::Primary`].
/// * A named alias that exists → that alias.
/// * A named alias that does not → `Err`, listing what is available. A silent
///   fallback to the parent would bill the flagship for work the operator
///   deliberately routed elsewhere, and nothing would say so.
/// * Nothing named → the pool's default, or the parent when it has none.
pub fn resolve_binding(
    pool: Option<&dyn ModelPool>,
    requested: Option<&str>,
) -> Result<Binding, String> {
    let Some(pool) = pool else {
        return Ok(Binding::Primary);
    };

    let requested = requested.map(str::trim).filter(|s| !s.is_empty());
    let Some(requested) = requested else {
        return Ok(match pool.default_alias() {
            // `primary` is reserved and is not a pool entry, so a default of
            // `primary` has to resolve the same way an explicit one does.
            // Returning `Alias("primary")` here sent the spawn to
            // `ModelPool::resolve`, which has no such entry and failed the call
            // — even though `validate` documents that default as legal.
            Some(alias) if alias.eq_ignore_ascii_case(PRIMARY_ALIAS) => Binding::Primary,
            Some(alias) => Binding::Alias(alias.to_string()),
            None => Binding::Primary,
        });
    };

    if requested.eq_ignore_ascii_case(PRIMARY_ALIAS) {
        return Ok(Binding::Primary);
    }
    if pool.entries().iter().any(|e| e.alias == requested) {
        return Ok(Binding::Alias(requested.to_string()));
    }

    let known: Vec<&str> = std::iter::once(PRIMARY_ALIAS)
        .chain(pool.entries().iter().map(|e| e.alias.as_str()))
        .collect();
    Err(format!(
        "unknown model {requested:?}; available: {}",
        known.join(", ")
    ))
}

/// The pool's entries with the default first, the rest in pool order.
fn ordered_entries(pool: &dyn ModelPool) -> Vec<&ModelPoolEntry> {
    let default = pool.default_alias();
    let mut entries: Vec<&ModelPoolEntry> = pool.entries().iter().collect();
    entries.sort_by_key(|entry| Some(entry.alias.as_str()) != default);
    entries
}

/// The `primary` choice, naming the model it binds to.
fn primary_label(caller_model: &str) -> String {
    format!("`{PRIMARY_ALIAS}` (= {caller_model})")
}

/// Render the `agent` tool's `model` parameter description for a pool.
///
/// Each entry with what it is for, the default first; then `primary`, which
/// always names the model it binds to, so "stay on what I am running" is
/// always expressible; then one note on reasoning effort.
#[must_use]
pub fn describe_parameter(pool: &dyn ModelPool, caller_model: &str) -> String {
    let mut out = String::from(
        "Model to run this subagent on. Choose by the kind of work, not by size — \
         a cheaper model that fits the task is the right answer.\n",
    );
    let default = pool.default_alias();
    for entry in ordered_entries(pool) {
        let marker = if default == Some(entry.alias.as_str()) {
            " [default]"
        } else {
            ""
        };
        let description = entry.description.trim();
        let separator = if description.is_empty() { "" } else { ": " };
        out.push_str(&format!(
            "\n- `{}` [{}]{marker}{separator}{description}",
            entry.alias, entry.model
        ));
    }
    let primary_default = if default.is_none() { " [default]" } else { "" };
    out.push_str(&format!(
        "\n- {}{primary_default}: your current model and reasoning effort",
        primary_label(caller_model)
    ));
    out.push_str("\n\nPool entries run at their own reasoning effort, not yours.");
    out
}

/// Render `agent_swarm`'s `model` parameter description: the choices on one
/// line; the `agent` tool's description says what each is for.
#[must_use]
pub fn summarize_parameter(pool: &dyn ModelPool, caller_model: &str) -> String {
    let default = pool.default_alias();
    let mut choices: Vec<String> = ordered_entries(pool)
        .into_iter()
        .map(|entry| {
            if default == Some(entry.alias.as_str()) {
                format!("`{}` [default]", entry.alias)
            } else {
                format!("`{}`", entry.alias)
            }
        })
        .collect();
    let primary_default = if default.is_none() { " [default]" } else { "" };
    choices.push(format!(
        "{}{primary_default}, your current model and reasoning effort",
        primary_label(caller_model)
    ));
    format!(
        "Model to run the item-spawned subagents on: {}. The `agent` tool's `model` \
         parameter says what each is for. Resumed subagents keep their own model.",
        choices.join(", ")
    )
}

/// Validate a pool before it is used.
///
/// Fails fast at startup rather than at spawn time: a broken pool discovered
/// mid-turn wastes the turn, and the operator is no longer watching.
pub fn validate(entries: &[ModelPoolEntry], default_alias: Option<&str>) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        if entry.alias.trim().is_empty() {
            return Err("a subagent model alias may not be empty".to_string());
        }
        if entry.alias.eq_ignore_ascii_case(PRIMARY_ALIAS) {
            return Err(format!(
                "subagent model alias {PRIMARY_ALIAS:?} is reserved for the calling model; \
                 rename this entry"
            ));
        }
        if entry.model.trim().is_empty() {
            return Err(format!(
                "subagent model {:?} names no model to run on",
                entry.alias
            ));
        }
        if !seen.insert(entry.alias.as_str()) {
            return Err(format!("duplicate subagent model alias {:?}", entry.alias));
        }
    }

    if let Some(default) = default_alias
        && !default.eq_ignore_ascii_case(PRIMARY_ALIAS)
        && !entries.iter().any(|e| e.alias == default)
    {
        return Err(format!(
            "subagent default_model {default:?} is not in the pool; available: {}",
            entries
                .iter()
                .map(|e| e.alias.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPool {
        entries: Vec<ModelPoolEntry>,
        default_alias: Option<String>,
    }

    impl TestPool {
        fn new(aliases: &[&str], default_alias: Option<&str>) -> Self {
            Self {
                entries: aliases
                    .iter()
                    .map(|a| ModelPoolEntry {
                        alias: (*a).to_string(),
                        model: format!("model-{a}"),
                        description: format!("for {a} work"),
                    })
                    .collect(),
                default_alias: default_alias.map(ToString::to_string),
            }
        }
    }

    impl ModelPool for TestPool {
        fn entries(&self) -> &[ModelPoolEntry] {
            &self.entries
        }
        fn default_alias(&self) -> Option<&str> {
            self.default_alias.as_deref()
        }
        fn resolve(&self, _alias: &str) -> Result<Arc<dyn Llm>, String> {
            Err("not needed for these tests".to_string())
        }
    }

    /// With no pool configured nothing changes: every sub-agent inherits the
    /// parent, and a `model` from a stale schema is ignored rather than failing.
    #[test]
    fn without_a_pool_everything_inherits_the_parent() {
        assert_eq!(resolve_binding(None, None), Ok(Binding::Primary));
        assert_eq!(resolve_binding(None, Some("fast")), Ok(Binding::Primary));
    }

    #[test]
    fn a_named_alias_binds_to_it() {
        let pool = TestPool::new(&["fast", "deep"], None);
        assert_eq!(
            resolve_binding(Some(&pool), Some("deep")),
            Ok(Binding::Alias("deep".to_string()))
        );
    }

    /// "Stay on what I am running" must always be expressible.
    #[test]
    fn primary_is_always_selectable() {
        let pool = TestPool::new(&["fast"], Some("fast"));
        for spelling in ["primary", "PRIMARY"] {
            assert_eq!(
                resolve_binding(Some(&pool), Some(spelling)),
                Ok(Binding::Primary)
            );
        }
    }

    /// **A `default_model` of `primary` must resolve like an explicit one.**
    /// `validate` accepts it — "sub-agents stay on my model" is a real thing to
    /// configure — and must not reach `ModelPool::resolve`, which has no
    /// `primary` entry: every spawn would fail with "no subagent model named
    /// \"primary\"".
    #[test]
    fn a_primary_default_resolves_to_the_caller_not_a_lookup() {
        for default in ["primary", "PRIMARY"] {
            let pool = TestPool::new(&["fast"], Some(default));
            assert_eq!(
                resolve_binding(Some(&pool), None),
                Ok(Binding::Primary),
                "default {default:?} must mean the caller's own model"
            );
        }
    }

    #[test]
    fn an_unnamed_model_takes_the_pool_default() {
        let pool = TestPool::new(&["fast", "deep"], Some("fast"));
        assert_eq!(
            resolve_binding(Some(&pool), None),
            Ok(Binding::Alias("fast".to_string()))
        );
        let no_default = TestPool::new(&["fast"], None);
        assert_eq!(
            resolve_binding(Some(&no_default), None),
            Ok(Binding::Primary)
        );
    }

    /// Blank and whitespace-only are "unnamed", not "an alias called nothing".
    #[test]
    fn a_blank_model_is_treated_as_unnamed() {
        let pool = TestPool::new(&["fast"], Some("fast"));
        for blank in ["", "   ", "\n"] {
            assert_eq!(
                resolve_binding(Some(&pool), Some(blank)),
                Ok(Binding::Alias("fast".to_string()))
            );
        }
    }

    /// **An unknown alias must fail, not fall back.** Falling back to the parent
    /// would bill the flagship for work the operator deliberately routed to a
    /// cheap model, and nothing would say it happened.
    #[test]
    fn an_unknown_alias_is_an_error_that_lists_the_options() {
        let pool = TestPool::new(&["fast", "deep"], Some("fast"));
        let err = resolve_binding(Some(&pool), Some("enormous")).unwrap_err();
        assert!(err.contains("enormous"), "{err}");
        assert!(err.contains("fast") && err.contains("deep"), "{err}");
        assert!(
            err.contains("primary"),
            "primary must be offered too: {err}"
        );
    }

    #[test]
    fn the_description_names_every_alias_and_marks_the_default() {
        let pool = TestPool::new(&["fast", "deep"], Some("deep"));
        let text = describe_parameter(&pool, "glm-5.3");
        assert!(
            text.contains("- `primary` (= glm-5.3): your current model and reasoning effort"),
            "{text}"
        );
        assert!(text.contains("for deep work"));
        assert!(
            text.contains("`deep` [model-deep] [default]"),
            "the default must be marked: {text}"
        );
        assert!(
            !text.contains("`fast` [model-fast] [default]"),
            "only one entry is the default: {text}"
        );
        let deep = text.find("`deep`").unwrap();
        let fast = text.find("`fast`").unwrap();
        let primary = text.find("`primary`").unwrap();
        assert!(
            deep < fast && fast < primary,
            "default first, primary last: {text}"
        );
        assert!(
            text.ends_with("Pool entries run at their own reasoning effort, not yours."),
            "{text}"
        );
    }

    /// When the pool has no default, the caller's own model is it.
    #[test]
    fn primary_is_marked_default_when_the_pool_has_none() {
        let text = describe_parameter(&TestPool::new(&["fast"], None), "kimi-for-coding");
        assert!(
            text.contains("`primary` (= kimi-for-coding) [default]"),
            "{text}"
        );
    }

    /// The swarm's description is one line naming the choices.
    #[test]
    fn the_swarm_summary_lists_the_choices_on_one_line() {
        let pool = TestPool::new(&["fast", "deep"], Some("deep"));
        let text = summarize_parameter(&pool, "glm-5.3");
        assert_eq!(
            text,
            "Model to run the item-spawned subagents on: `deep` [default], `fast`, `primary` \
             (= glm-5.3), your current model and reasoning effort. The `agent` tool's `model` \
             parameter says what each is for. Resumed subagents keep their own model."
        );
        assert!(!text.contains('\n'));
    }

    /// An entry may describe nothing; it must still render cleanly.
    #[test]
    fn an_entry_without_a_description_renders_without_a_dangling_colon() {
        let pool = TestPool {
            entries: vec![ModelPoolEntry {
                alias: "bare".to_string(),
                model: "m".to_string(),
                description: String::new(),
            }],
            default_alias: None,
        };
        let text = describe_parameter(&pool, "main");
        assert!(text.contains("`bare` [m]"), "{text}");
        assert!(!text.contains("[m]: "), "no dangling separator: {text}");
    }

    // ---- validation -------------------------------------------------------

    fn entry(alias: &str, model: &str) -> ModelPoolEntry {
        ModelPoolEntry {
            alias: alias.to_string(),
            model: model.to_string(),
            description: String::new(),
        }
    }

    #[test]
    fn a_well_formed_pool_validates() {
        let entries = vec![entry("fast", "glm-5.3-flash"), entry("deep", "glm-5.3")];
        assert_eq!(validate(&entries, Some("fast")), Ok(()));
        assert_eq!(validate(&entries, None), Ok(()));
        assert_eq!(validate(&[], None), Ok(()));
    }

    /// The reserved alias would silently redirect every sub-agent that asked to
    /// stay on the caller's model.
    #[test]
    fn the_primary_alias_may_not_be_claimed() {
        for alias in ["primary", "Primary", "PRIMARY"] {
            assert!(
                validate(&[entry(alias, "m")], None)
                    .unwrap_err()
                    .contains("reserved")
            );
        }
    }

    #[test]
    fn a_default_outside_the_pool_is_rejected() {
        let err = validate(&[entry("fast", "m")], Some("nope")).unwrap_err();
        assert!(err.contains("nope") && err.contains("fast"), "{err}");
    }

    /// `primary` is always a valid default even though it is not an entry.
    #[test]
    fn primary_is_a_valid_default() {
        assert_eq!(validate(&[entry("fast", "m")], Some("primary")), Ok(()));
    }

    #[test]
    fn duplicate_and_empty_entries_are_rejected() {
        let dup = vec![entry("fast", "a"), entry("fast", "b")];
        assert!(validate(&dup, None).unwrap_err().contains("duplicate"));
        assert!(validate(&[entry("", "m")], None).is_err());
        assert!(
            validate(&[entry("fast", "  ")], None)
                .unwrap_err()
                .contains("names no model")
        );
    }
}
