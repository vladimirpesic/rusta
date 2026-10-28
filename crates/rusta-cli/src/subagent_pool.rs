//! The `[subagent]` config section, and the model pool it builds.
//!
//! `agent-core` defines *what* a pool is; this is where one comes from. Building
//! a provider means resolving an endpoint from a provider name, a billing plan
//! and a region, and finding the right API key for it — all of which belong to
//! the CLI, which is why [`agent_core::model_pool::ModelPool`] is a trait rather
//! than a concrete type.
//!
//! ```toml
//! [subagent]
//! default_model = "fast"
//!
//! [subagent.models.fast]
//! model = "glm-5.3-flash"
//! description = "Locating files, reading configs, simple lookups."
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;

use agent_core::agent_loop::llm::Llm;
use agent_core::model_pool::{ModelPool, ModelPoolEntry};
use lm_bridge::providers::openai_compat::HttpTimeouts;
use serde::{Deserialize, Serialize};

use crate::cli::Cli;

/// One `[subagent.models.<alias>]` entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubagentModel {
    /// The model id to run on. Required.
    pub model: String,
    /// What this entry is for. Rendered into the tool description, so it is the
    /// text the model actually chooses on.
    pub description: String,
    /// Provider for this entry. Defaults to the session's provider, so a pool of
    /// same-vendor models needs no repetition.
    pub provider: Option<String>,
    /// Reasoning effort for this entry. A cheap model at `low` effort is often
    /// the whole point of routing work to it.
    pub reasoning_effort: Option<String>,
}

/// The `[subagent]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SubagentConfig {
    /// Alias used when the model names none. `primary` keeps the parent's model.
    pub default_model: Option<String>,
    /// The pool, keyed by alias. `BTreeMap` so the order the model sees is the
    /// alphabetical order of the aliases rather than whatever the TOML parser
    /// happened to produce — a tool description that reshuffles between runs is
    /// a needless source of nondeterminism.
    pub models: BTreeMap<String, SubagentModel>,
}

impl SubagentConfig {
    /// Whether anything is configured. An empty section is not a pool.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.models.is_empty() && self.default_model.is_none()
    }

    /// The entries in the shape `agent-core` wants.
    #[must_use]
    pub fn entries(&self) -> Vec<ModelPoolEntry> {
        self.models
            .iter()
            .map(|(alias, m)| ModelPoolEntry {
                alias: alias.clone(),
                model: m.model.clone(),
                description: m.description.clone(),
            })
            .collect()
    }

    /// Validate the section, naming the offending entry.
    ///
    /// Called at startup: a pool that cannot resolve is better discovered before
    /// the first prompt than three turns into a delegation.
    pub fn validate(&self) -> Result<(), String> {
        agent_core::model_pool::validate(&self.entries(), self.default_model.as_deref())
    }
}

/// A [`ModelPool`] that builds providers the same way the main session does.
pub struct CliModelPool {
    entries: Vec<ModelPoolEntry>,
    default_alias: Option<String>,
    models: BTreeMap<String, SubagentModel>,
    /// A snapshot of the session's CLI settings. Each entry overrides the parts
    /// it names — model, provider, effort — and inherits the rest, so a pool of
    /// same-vendor models needs no endpoint or key repeated per entry.
    base: Cli,
    timeouts: HttpTimeouts,
}

impl CliModelPool {
    /// Build a pool, or `None` when nothing is configured.
    pub fn new(
        config: &SubagentConfig,
        base: &Cli,
        timeouts: HttpTimeouts,
    ) -> Result<Option<Self>, String> {
        if config.is_empty() {
            return Ok(None);
        }
        config.validate()?;
        Ok(Some(Self {
            entries: config.entries(),
            default_alias: config.default_model.clone(),
            models: config.models.clone(),
            base: base.clone(),
            timeouts,
        }))
    }

    /// The settings one pool entry runs under.
    ///
    /// Starts from the session's own, so the endpoint, key, billing plan and
    /// region are inherited, and overrides only what the entry names. Split out
    /// from [`ModelPool::resolve`] so the inheritance can be tested without
    /// building a provider: an entry's `reasoning_effort` that is parsed but
    /// never reaches the request is invisible at runtime — the sub-agent still
    /// answers, just at the wrong effort and the wrong price.
    fn entry_cli(base: &Cli, alias: &str, entry: &SubagentModel) -> Cli {
        let mut cli = base.clone();
        // A pool entry's conversations share their own cache bucket, apart from
        // the session's: `<session-key>:<alias>`.
        if let Some(key) = base.cache_key() {
            cli.session = None;
            cli.run_id = Some(format!("{key}:{alias}"));
        }
        if let Some(effort) = &entry.reasoning_effort {
            cli.reasoning_effort = Some(effort.clone());
        }
        // A different provider implies a different endpoint and key, so the
        // session's must not carry over — the same reasoning that stops
        // `/provider glm` inheriting a Kimi key.
        if entry.provider.is_some() {
            cli.base_url = None;
            cli.api_key = None;
        }
        cli
    }
}

impl ModelPool for CliModelPool {
    fn entries(&self) -> &[ModelPoolEntry] {
        &self.entries
    }

    fn default_alias(&self) -> Option<&str> {
        self.default_alias.as_deref()
    }

    fn resolve(&self, alias: &str) -> Result<Arc<dyn Llm>, String> {
        let entry = self
            .models
            .get(alias)
            .ok_or_else(|| format!("no subagent model named {alias:?}"))?;

        let cli = Self::entry_cli(&self.base, alias, entry);

        let provider = crate::build_provider_for(
            &cli,
            entry.provider.clone(),
            Some(entry.model.clone()),
            self.timeouts,
        )
        .map_err(|e| e.to_string())?;

        Ok(Arc::new(
            crate::commands::LlmOptions::from_cli(&cli).build_llm(provider),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::LlmOptions;
    use clap::Parser;

    /// A session's CLI settings, as parsed with no arguments.
    fn session_cli() -> Cli {
        Cli::parse_from(["rusta"])
    }

    fn config(toml_src: &str) -> SubagentConfig {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            subagent: SubagentConfig,
        }
        toml::from_str::<Wrapper>(toml_src)
            .expect("must parse")
            .subagent
    }

    #[test]
    fn an_absent_section_is_no_pool() {
        assert!(config("").is_empty());
        assert!(config("[subagent]\n").is_empty());
    }

    #[test]
    fn entries_are_read_in_alias_order() {
        let c = config(
            "[subagent.models.zebra]\nmodel = \"m-z\"\n[subagent.models.alpha]\nmodel = \"m-a\"\n",
        );
        assert_eq!(
            c.entries()
                .iter()
                .map(|e| e.alias.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "zebra"],
            "the order the model sees must not depend on the parser"
        );
    }

    /// A `default_model` naming nothing is the misconfiguration most likely to
    /// go unnoticed, because everything still runs — on the wrong model.
    #[test]
    fn a_default_outside_the_pool_fails_validation() {
        let c =
            config("[subagent]\ndefault_model = \"typo\"\n[subagent.models.fast]\nmodel = \"m\"\n");
        let err = c.validate().unwrap_err();
        assert!(err.contains("typo") && err.contains("fast"), "{err}");
    }

    #[test]
    fn the_reserved_alias_fails_validation() {
        assert!(
            config("[subagent.models.primary]\nmodel = \"m\"\n")
                .validate()
                .unwrap_err()
                .contains("reserved")
        );
    }

    #[test]
    fn an_entry_without_a_model_fails_validation() {
        assert!(
            config("[subagent.models.fast]\ndescription = \"quick\"\n")
                .validate()
                .unwrap_err()
                .contains("names no model")
        );
    }

    /// A lone `default_model = "primary"` is legal: it means "sub-agents stay on
    /// my model", which is the default anyway but worth being able to state.
    #[test]
    fn a_lone_primary_default_is_valid() {
        let c = config("[subagent]\ndefault_model = \"primary\"\n");
        assert!(!c.is_empty());
        assert_eq!(c.validate(), Ok(()));
    }

    /// An entry's effort must actually reach the request that the sub-agent
    /// makes. Parsed-but-unused is the silent version of this bug: routing to a
    /// cheap model at `low` effort is usually the entire reason for the entry.
    #[test]
    fn an_entry_overrides_the_effort_it_names() {
        let mut base = session_cli();
        base.reasoning_effort = Some("high".to_string());
        let entry = SubagentModel {
            model: "glm-5.3-flash".to_string(),
            reasoning_effort: Some("low".to_string()),
            ..Default::default()
        };
        let derived = CliModelPool::entry_cli(&base, "fast", &entry);
        assert_eq!(derived.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(
            LlmOptions::from_cli(&derived).reasoning_effort.as_deref(),
            Some("low"),
            "the override must survive into the options the LLM is built with"
        );
    }

    /// An entry that names no effort inherits the session's, rather than
    /// silently dropping to a provider default.
    #[test]
    fn an_entry_without_an_effort_inherits_the_session() {
        let mut base = session_cli();
        base.reasoning_effort = Some("high".to_string());
        let entry = SubagentModel {
            model: "glm-5.3".to_string(),
            ..Default::default()
        };
        let derived = CliModelPool::entry_cli(&base, "fast", &entry);
        assert_eq!(derived.reasoning_effort.as_deref(), Some("high"));
    }

    /// A pool entry keys its prompt cache apart from the session's, and does so
    /// whether or not the session was named.
    #[test]
    fn an_entry_gets_its_own_prompt_cache_key() {
        let mut base = session_cli();
        base.run_id = Some("01run".to_string());
        let entry = SubagentModel {
            model: "glm-5.3-flash".to_string(),
            ..Default::default()
        };
        let derived = CliModelPool::entry_cli(&base, "fast", &entry);
        assert_eq!(
            LlmOptions::from_cli(&derived).prompt_cache_key.as_deref(),
            Some("01run:fast")
        );

        base.session = Some("named".to_string());
        let derived = CliModelPool::entry_cli(&base, "deep", &entry);
        assert_eq!(derived.cache_key().as_deref(), Some("named:deep"));
    }

    /// A cross-provider entry must not inherit the session's endpoint or key.
    #[test]
    fn a_cross_provider_entry_drops_the_session_endpoint_and_key() {
        let mut base = session_cli();
        base.base_url = Some("https://api.z.ai/x".to_string());
        base.api_key = Some("zai-secret".to_string());
        let entry = SubagentModel {
            model: "kimi-k3".to_string(),
            provider: Some("kimi".to_string()),
            ..Default::default()
        };
        let derived = CliModelPool::entry_cli(&base, "fast", &entry);
        assert_eq!(
            derived.base_url, None,
            "a Kimi entry must not keep a Z.AI endpoint"
        );
        assert_eq!(derived.api_key, None, "nor its key");
    }

    #[test]
    fn a_well_formed_pool_validates() {
        let c = config(
            "[subagent]\ndefault_model = \"fast\"\n[subagent.models.fast]\nmodel = \"glm-5.3-flash\"\nreasoning_effort = \"low\"\n",
        );
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(
            c.models["fast"].reasoning_effort.as_deref(),
            Some("low"),
            "per-entry effort is the point of routing to a cheap model"
        );
    }
}
