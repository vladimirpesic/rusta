use std::path::{Path, PathBuf};

use agent_core::permissions::PolicyRule;
use mcp_client::McpServerConfig;
use serde::Deserialize;

use crate::endpoints::{Billing, Region};

/// A project's config file, read from the directory Rusta starts in.
pub const PROJECT_FILE: &str = ".rusta.toml";

/// Where a config file sits among the layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The user's own config (`~/.config/rusta/config.toml` on Linux).
    User,
    /// [`PROJECT_FILE`] in the current directory. It comes with the
    /// repository, so workspace trust decides what of it applies
    /// ([`crate::trust`]).
    Project,
    /// A file named for this invocation (`--config`, `RUSTA_CONFIG`).
    Explicit,
}

/// Configuration loaded from file.
///
/// Values here are overridden by the corresponding CLI arguments or
/// environment variables.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct Config {
    /// Default provider (`kimi` or `glm`).
    pub provider: Option<String>,
    /// Default model identifier.
    pub model: Option<String>,
    /// Custom API base URL.
    pub base_url: Option<String>,

    /// Which quota the API key is billed against: `subscription` or `api`.
    #[serde(default)]
    pub billing: Option<Billing>,

    /// Regional deployment: `global` or `china`.
    #[serde(default)]
    pub region: Option<Region>,
    /// API key.
    pub api_key: Option<String>,
    /// The environment variable holding the API key, in place of `api_key`
    /// (which it excludes). The variable must be set and non-empty.
    pub api_key_env: Option<String>,
    /// Extra HTTP headers sent with every model request, below the
    /// provider's own. `RUSTA_CUSTOM_HEADERS` supplies more; these win.
    #[serde(default)]
    pub custom_headers: std::collections::BTreeMap<String, String>,
    /// Path to conversation records (JSONL).
    pub records: Option<PathBuf>,
    /// Default permission mode (`default`, `accept_edits`, `bypass_permissions`).
    pub permission_mode: Option<String>,
    /// Start sessions in plan mode (`--plan`). Not applied to a headless run
    /// that nobody can review a plan in.
    pub plan: Option<bool>,
    /// MCP servers to connect to for additional tools.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,

    /// Global MCP defaults, applied to every server that does not set its own —
    /// including the auto-detected ones, which have no `[[mcp_servers]]` entry
    /// to edit and so cannot be tuned any other way.
    #[serde(default)]
    pub mcp: mcp_client::McpDefaults,

    /// Models sub-agents may be bound to. Empty means every sub-agent inherits
    /// the parent, which is the behaviour without this section.
    #[serde(default)]
    pub subagent: crate::subagent_pool::SubagentConfig,
    /// URL template for the built-in web_search tool.
    pub search_url: Option<String>,
    /// Additional paths outside cwd that tools are allowed to access.
    #[serde(default)]
    pub allowed_paths: Vec<std::path::PathBuf>,
    /// Request timeout in seconds.
    pub timeout: Option<u64>,
    /// Reasoning effort level (`low`, `medium`, `high`).
    pub reasoning_effort: Option<String>,
    /// Keep reasoning/thinking content in the response.
    pub thinking_keep: Option<bool>,
    /// How long Kimi's platform keeps a written prompt-cache entry, `5m` or
    /// `1h`; unset, `1h` in an interactive session and `5m` in a headless
    /// run.
    pub prompt_cache_ttl: Option<lm_bridge::provider::PromptCacheTtl>,
    /// Pin the wire field an OpenAI-compatible endpoint uses for reasoning:
    /// `reasoning_content` (default), `reasoning`, or `reasoning_details`.
    /// Leave unset to detect it from the endpoint's own responses.
    pub reasoning_key: Option<String>,
    /// Maximum attempts for a single model call before its error is surfaced.
    /// Counts the first attempt, so `1` disables retrying. Defaults to 10.
    pub max_attempts_per_step: Option<u32>,
    /// Retry failing model calls without limit, for unattended runs that should
    /// wait out an outage or a spent quota window rather than fail.
    pub infinite_retry: Option<bool>,
    /// How long a delegated sub-agent may run, in milliseconds, before it is
    /// cancelled. `0` removes the deadline. Defaults to two hours.
    pub subagent_timeout_ms: Option<u64>,
    /// How long each `agent_swarm` member may run, in milliseconds. `0`
    /// removes the deadline. Defaults to two hours.
    pub swarm_timeout_ms: Option<u64>,
    /// How many `agent_swarm` members run at once. Unset runs them all.
    pub swarm_max_concurrency: Option<usize>,
    /// The repeat breaker: reminders for a tool call repeated across steps,
    /// and a stop with a handoff after a long run of one call. Defaults to
    /// true.
    pub repeat_breaker: Option<bool>,
    /// Title an untitled session after a completed turn in the REPL or the
    /// TUI, from its first exchange.
    /// Defaults to true.
    pub auto_session_title: Option<bool>,
    /// Ask before dangerous shell commands in the default and accept-edits
    /// modes. Defaults to true; false turns off the built-in prompts before
    /// writing secrets and before destructive git commands too.
    pub dangerous_command_guard: Option<bool>,
    /// What a headless run does about background work once its prompt is
    /// answered: `steer` (default), `drain` or `exit`.
    pub headless_background_mode: Option<crate::cli::HeadlessBackgroundMode>,
    /// Longest time, in seconds, a headless run keeps waiting or steering
    /// after its prompt. Unset: no ceiling.
    pub headless_wait_ceiling_s: Option<u64>,
    /// Most follow-up turns a `steer` headless run takes. Defaults to 100000.
    pub headless_max_turns: Option<u32>,
    /// Maximum number of tokens to generate.
    pub max_tokens: Option<u32>,
    /// Maximum model round-trips (steps) per agent turn before stopping with
    /// "Max turns exceeded". Defaults to 100 when unset.
    pub max_turns: Option<u32>,
    /// How deep sub-agent delegation may nest. `0` forbids it. Defaults to 1.
    pub max_subagent_depth: Option<usize>,
    /// How long a model request may go silent before it is abandoned, in
    /// seconds. Resets on every chunk. Defaults to 300; `0` disables it.
    pub idle_timeout: Option<u64>,
    /// Sampling temperature.
    pub temperature: Option<f32>,
    /// Nucleus-sampling top-p value.
    pub top_p: Option<f32>,
    /// Enable model reasoning/thinking mode.
    pub thinking: Option<bool>,
    /// Additional project context files to inject into the system prompt.
    #[serde(default)]
    pub project_context_files: Vec<std::path::PathBuf>,
    /// Additional directories to include as context (listings + AGENTS.md).
    #[serde(default)]
    pub additional_dirs: Vec<std::path::PathBuf>,
    /// Token budget for the project instruction files in the system prompt
    /// (default 8192).
    pub project_context_budget_tokens: Option<u64>,
    /// Maximum number of concurrently running background tasks.
    pub background_max_running_tasks: Option<usize>,
    /// Budget for one tool result, in characters, before it is cut and the full
    /// output saved to a spill file. Defaults to 50000; `0` disables the cap.
    pub tool_output_max_chars: Option<usize>,
    /// The most characters one `read_file` call may return when it asks for a
    /// larger `max_chars`. Defaults to 500000; `0` removes the ceiling.
    pub read_max_chars: Option<usize>,
    /// Model requests one context compaction may make, retries and shrinks
    /// included, before it falls back to evicting old messages. Defaults to 5.
    pub compaction_max_attempts: Option<u32>,
    /// Raw-byte budget for one image `read_media` sends. Defaults to 262144;
    /// `0` sets no budget.
    pub image_read_byte_budget: Option<usize>,
    /// Send video inline on a generic OpenAI-compatible endpoint whose model
    /// takes it. Defaults to false.
    pub inline_video: Option<bool>,
    /// Additional directories to scan for Markdown skill files.
    #[serde(default)]
    pub skill_dirs: Vec<std::path::PathBuf>,
    /// Permission policy configuration.
    pub permissions: Option<PermissionsConfig>,
    /// User hooks fired on session and turn events. User config only: a
    /// project file's hooks are refused (see `crate::trust`), because a
    /// cloned repository must not run code.
    #[serde(default)]
    pub hooks: Vec<agent_core::user_hooks::UserHookRule>,
}

/// Permission policy configuration.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub rules: Vec<PolicyRule>,
}

/// Overwrite `dst` only when the higher-precedence layer actually set a value.
fn overlay_opt<T>(dst: &mut Option<T>, higher: Option<T>) {
    if higher.is_some() {
        *dst = higher;
    }
}

/// Normalize a provider name so aliases compare equal. `zai` is `glm`.
fn normalized_provider(provider: &str) -> &str {
    if provider == "zai" { "glm" } else { provider }
}

/// The provider a config currently names, normalized. `None` — no provider set
/// yet — is treated as its own value so the first layer to name one is not
/// mistaken for a change.
fn current_provider(config: &Config) -> &str {
    config.provider.as_deref().map_or("", normalized_provider)
}

/// Concatenate `higher` before `lower`, dropping later duplicates.
///
/// Higher-precedence entries come first because both list consumers are
/// first-match-wins: permission rules stop at the first match, and MCP server
/// injection skips a name that is already present.
fn overlay_list<T, K, F>(lower: Vec<T>, higher: Vec<T>, key: F) -> Vec<T>
where
    K: std::hash::Hash + Eq,
    F: Fn(&T) -> K,
{
    let mut seen = std::collections::HashSet::new();
    higher
        .into_iter()
        .chain(lower)
        .filter(|item| seen.insert(key(item)))
        .collect()
}

/// A variable's value as a positive number of milliseconds.
///
/// A malformed or zero value is ignored rather than fatal: a stray export must
/// not stop a session from starting, and zero is not a timeout anyone means.
fn parse_millis(value: &str) -> Option<u64> {
    value.parse::<u64>().ok().filter(|v| *v > 0)
}

/// A variable's value as a boolean, on [`parse_millis`]'s terms: a malformed
/// value is ignored rather than fatal.
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Config {
    /// Apply `RUSTA_MCP_*` overrides on top of the merged `[mcp]` section.
    ///
    /// Precedence ends up as per-server > environment > config file > built-in,
    /// matching how every other Rusta setting resolves.
    pub fn apply_mcp_env_overrides(&mut self) {
        self.apply_mcp_env_overrides_with(crate::endpoints::env_non_empty);
    }

    /// [`Self::apply_mcp_env_overrides`], reading variables through `env`
    /// (on [`crate::endpoints::env_non_empty`]'s terms), injected so tests
    /// need not mutate the process.
    fn apply_mcp_env_overrides_with(&mut self, env: impl Fn(&str) -> Option<String>) {
        if let Some(ms) = env("RUSTA_MCP_STARTUP_TIMEOUT_MS").and_then(|v| parse_millis(&v)) {
            self.mcp.startup_timeout_ms = Some(ms);
        }
        if let Some(ms) = env("RUSTA_MCP_TOOL_TIMEOUT_MS").and_then(|v| parse_millis(&v)) {
            self.mcp.tool_timeout_ms = Some(ms);
        }
        if let Some(deferred) = env("RUSTA_MCP_DEFAULT_DEFERRED").and_then(|v| parse_bool(&v)) {
            self.mcp.default_deferred = Some(deferred);
        }
    }

    /// Fill in the timeouts of every server that did not name its own.
    pub fn apply_mcp_timeout_defaults(&mut self) {
        let defaults = self.mcp;
        for server in &mut self.mcp_servers {
            server.apply_timeout_defaults(&defaults);
        }
    }

    /// Load configuration by **layering** every readable file in `sources`.
    ///
    /// `sources` is ordered lowest-precedence first; each later file overlays
    /// the ones before it. Missing files are skipped, an unparseable one is an
    /// error rather than a silent fallback. A [`Source::Project`] layer goes
    /// through `project` first, given the layers below it and the file's
    /// path, and what it returns is what applies.
    ///
    /// Layering (rather than first-file-wins) is what lets credentials live in
    /// the user config while a project keeps its own permission rules and MCP
    /// servers. Under first-file-wins the global config written by
    /// `rusta init` shadowed `.rusta.toml` entirely, so a project's rules
    /// silently stopped applying the moment a user ran setup.
    pub fn load_sources(
        sources: &[(Source, PathBuf)],
        mut project: impl FnMut(&Self, &Path, Self) -> anyhow::Result<Self>,
    ) -> anyhow::Result<Self> {
        let mut merged = Self::default();
        for (source, path) in sources {
            let Some(layer) = Self::read_layer(path)? else {
                continue;
            };
            let layer = if *source == Source::Project {
                project(&merged, path, layer)?
            } else {
                layer
            };
            merged.overlay(layer);
        }
        Ok(merged)
    }

    /// Layer `paths`, every file applying in full.
    #[cfg(test)]
    pub fn load_from(paths: &[PathBuf]) -> anyhow::Result<Self> {
        let sources: Vec<_> = paths.iter().map(|p| (Source::User, p.clone())).collect();
        Self::load_sources(&sources, |_, _, layer| Ok(layer))
    }

    /// Read one config file: `None` when it does not exist.
    pub fn read_layer(path: &Path) -> anyhow::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read config {}: {e}", path.display()))?;
        let layer: Self = toml::from_str(&contents)
            .map_err(|e| anyhow::anyhow!("failed to parse config {}: {e}", path.display()))?;
        if layer.api_key.is_some() && layer.api_key_env.is_some() {
            anyhow::bail!(
                "config {} sets both api_key and api_key_env; keep one",
                path.display()
            );
        }
        for rule in &layer.hooks {
            rule.validate().map_err(|e| {
                anyhow::anyhow!("config {}: invalid [[hooks]] entry: {e}", path.display())
            })?;
        }
        Ok(Some(layer))
    }

    /// Apply `higher` on top of `self`; `higher` wins wherever it sets a value.
    fn overlay(&mut self, higher: Self) {
        // `api_key`, `base_url`, and `model` belong to the *provider* of the
        // layer that set them. When a higher layer names a different provider —
        // a project `.rusta.toml` pinning `provider = "glm"` over a user config
        // holding a Kimi key — the lower layer's credential and endpoint are for
        // the other provider and must not carry across, or a Kimi key is sent to
        // Z.AI and rejected as if it were wrong. (`zai` is an alias for `glm`, so
        // it is not a change.) Mirrors `main::apply_config`'s `same_provider` rule.
        let provider_changed = higher
            .provider
            .as_deref()
            .is_some_and(|next| normalized_provider(next) != current_provider(self));
        if provider_changed {
            self.api_key = None;
            self.api_key_env = None;
            self.base_url = None;
            self.model = None;
            self.custom_headers.clear();
        }
        // The two ways of giving the key exclude each other: a layer that
        // names one replaces the other.
        if higher.api_key.is_some() {
            self.api_key_env = None;
        }
        if higher.api_key_env.is_some() {
            self.api_key = None;
        }
        overlay_opt(&mut self.api_key_env, higher.api_key_env);
        self.custom_headers.extend(higher.custom_headers);

        overlay_opt(&mut self.provider, higher.provider);
        overlay_opt(&mut self.model, higher.model);
        overlay_opt(&mut self.base_url, higher.base_url);
        overlay_opt(&mut self.billing, higher.billing);
        overlay_opt(&mut self.region, higher.region);
        overlay_opt(&mut self.api_key, higher.api_key);
        overlay_opt(&mut self.records, higher.records);
        overlay_opt(&mut self.permission_mode, higher.permission_mode);
        overlay_opt(&mut self.plan, higher.plan);
        overlay_opt(&mut self.search_url, higher.search_url);
        overlay_opt(&mut self.timeout, higher.timeout);
        overlay_opt(&mut self.reasoning_effort, higher.reasoning_effort);
        overlay_opt(&mut self.thinking_keep, higher.thinking_keep);
        overlay_opt(&mut self.prompt_cache_ttl, higher.prompt_cache_ttl);
        overlay_opt(&mut self.max_tokens, higher.max_tokens);
        overlay_opt(&mut self.reasoning_key, higher.reasoning_key);
        // A project `.rusta.toml` raising `max_turns` for a long autonomous
        // task overrides the user config's value like any other setting.
        overlay_opt(&mut self.max_turns, higher.max_turns);
        overlay_opt(&mut self.max_subagent_depth, higher.max_subagent_depth);
        overlay_opt(&mut self.idle_timeout, higher.idle_timeout);
        overlay_opt(
            &mut self.max_attempts_per_step,
            higher.max_attempts_per_step,
        );
        overlay_opt(&mut self.infinite_retry, higher.infinite_retry);
        overlay_opt(&mut self.subagent_timeout_ms, higher.subagent_timeout_ms);
        overlay_opt(&mut self.swarm_timeout_ms, higher.swarm_timeout_ms);
        overlay_opt(
            &mut self.swarm_max_concurrency,
            higher.swarm_max_concurrency,
        );
        overlay_opt(&mut self.repeat_breaker, higher.repeat_breaker);
        overlay_opt(&mut self.auto_session_title, higher.auto_session_title);
        overlay_opt(
            &mut self.dangerous_command_guard,
            higher.dangerous_command_guard,
        );
        overlay_opt(
            &mut self.headless_background_mode,
            higher.headless_background_mode,
        );
        overlay_opt(
            &mut self.headless_wait_ceiling_s,
            higher.headless_wait_ceiling_s,
        );
        overlay_opt(&mut self.headless_max_turns, higher.headless_max_turns);
        overlay_opt(
            &mut self.mcp.startup_timeout_ms,
            higher.mcp.startup_timeout_ms,
        );
        overlay_opt(&mut self.mcp.tool_timeout_ms, higher.mcp.tool_timeout_ms);
        overlay_opt(&mut self.mcp.default_deferred, higher.mcp.default_deferred);
        // The pool replaces wholesale rather than merging entry by entry: a
        // project that defines its own routing means *that* routing, and half of
        // it inherited from the user config would be neither.
        if !higher.subagent.is_empty() {
            self.subagent = higher.subagent;
        }
        overlay_opt(&mut self.temperature, higher.temperature);
        overlay_opt(&mut self.top_p, higher.top_p);
        overlay_opt(&mut self.thinking, higher.thinking);
        overlay_opt(
            &mut self.project_context_budget_tokens,
            higher.project_context_budget_tokens,
        );
        overlay_opt(
            &mut self.background_max_running_tasks,
            higher.background_max_running_tasks,
        );
        overlay_opt(
            &mut self.tool_output_max_chars,
            higher.tool_output_max_chars,
        );
        overlay_opt(&mut self.read_max_chars, higher.read_max_chars);
        overlay_opt(
            &mut self.compaction_max_attempts,
            higher.compaction_max_attempts,
        );
        overlay_opt(
            &mut self.image_read_byte_budget,
            higher.image_read_byte_budget,
        );
        overlay_opt(&mut self.inline_video, higher.inline_video);

        // A server declared in both layers is one server: the higher-precedence
        // declaration wins, and auto-injection still sees the name as taken.
        self.mcp_servers = overlay_list(
            std::mem::take(&mut self.mcp_servers),
            higher.mcp_servers,
            |s| s.name.clone(),
        );

        for (dst, src) in [
            (&mut self.allowed_paths, higher.allowed_paths),
            (
                &mut self.project_context_files,
                higher.project_context_files,
            ),
            (&mut self.additional_dirs, higher.additional_dirs),
            (&mut self.skill_dirs, higher.skill_dirs),
        ] {
            *dst = overlay_list(std::mem::take(dst), src, Clone::clone);
        }

        // Rules are evaluated in order and the first match wins, so the
        // higher-precedence layer's rules must be evaluated first.
        if let Some(higher_perms) = higher.permissions {
            let lower_rules = self.permissions.take().map(|p| p.rules).unwrap_or_default();
            let mut rules = higher_perms.rules;
            rules.extend(lower_rules);
            self.permissions = Some(PermissionsConfig { rules });
        }

        // Every matching hook runs, so precedence does not matter; the user's
        // rules and the explicit layer's simply accumulate.
        self.hooks.extend(higher.hooks);
    }

    /// Config files in **increasing** precedence order.
    ///
    /// 1. The platform config directory (`~/.config/rusta/config.toml` on
    ///    Linux) — user-wide defaults, where `rusta init` stores credentials.
    /// 2. `.rusta.toml` in the current directory — project settings, which
    ///    override the user config key by key, as far as workspace trust
    ///    allows.
    /// 3. `RUSTA_CONFIG` — an explicit choice for this invocation, so it wins.
    pub fn default_sources() -> Vec<(Source, PathBuf)> {
        let mut sources = Vec::new();
        if let Some(config_dir) = agent_core::user_dirs::config_dir() {
            sources.push((Source::User, config_dir.join("rusta").join("config.toml")));
        }
        sources.push((Source::Project, PathBuf::from(PROJECT_FILE)));
        if let Ok(env_path) = std::env::var("RUSTA_CONFIG") {
            sources.push((Source::Explicit, PathBuf::from(env_path)));
        }
        sources
    }
}

/// Expand a leading `~` in a path to the user's home directory.
pub fn expand_tilde(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if let Ok(home) = std::env::var("HOME")
        && path.starts_with("~")
    {
        return PathBuf::from(home).join(path.strip_prefix("~").unwrap_or(path));
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What this layering exists for: `rusta init` writes the API key to the
    /// user config, and a project keeps its own permission rules and MCP
    /// servers. Under first-file-wins the user config would shadow the project
    /// file outright, so those rules would silently stop applying.
    #[test]
    fn project_config_layers_over_the_user_config() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.toml");
        let project = dir.path().join(".rusta.toml");

        std::fs::write(
            &user,
            r#"provider = "kimi"
billing = "subscription"
api_key = "sk-user-key"
temperature = 0.2
"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"temperature = 0.9

[[mcp_servers]]
name = "devscriptor"
command = "/opt/devscriptor"

[[permissions.rules]]
name = "project-allow"
tool = "read_file"
action = "allow"
"#,
        )
        .unwrap();

        let config = Config::load_from(&[user, project]).unwrap();

        // The credential from the user config survives...
        assert_eq!(config.api_key.as_deref(), Some("sk-user-key"));
        assert_eq!(config.provider.as_deref(), Some("kimi"));
        // ...the project's settings are applied...
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.permissions.expect("rules").rules.len(), 1);
        // ...and the project wins where both set the same key.
        assert_eq!(config.temperature, Some(0.9));
    }

    /// A server declared in both layers is one server, and the higher layer's
    /// declaration is the one that survives.
    #[test]
    fn a_duplicate_mcp_server_resolves_to_the_higher_layer() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.toml");
        let project = dir.path().join(".rusta.toml");
        std::fs::write(
            &user,
            "[[mcp_servers]]\nname = \"shared\"\ncommand = \"/usr/bin/global\"\n",
        )
        .unwrap();
        std::fs::write(
            &project,
            "[[mcp_servers]]\nname = \"shared\"\ncommand = \"/opt/project\"\n",
        )
        .unwrap();

        let config = Config::load_from(&[user, project]).unwrap();
        assert_eq!(config.mcp_servers.len(), 1, "one name is one server");
        assert_eq!(
            config.mcp_servers[0].command.as_deref(),
            Some("/opt/project")
        );
    }

    /// Rules are first-match-wins, so the higher layer's must be evaluated first.
    #[test]
    fn higher_layer_permission_rules_are_evaluated_first() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.toml");
        let project = dir.path().join(".rusta.toml");
        std::fs::write(
            &user,
            "[[permissions.rules]]\nname = \"user-rule\"\ntool = \"bash\"\naction = \"deny\"\n",
        )
        .unwrap();
        std::fs::write(
            &project,
            "[[permissions.rules]]\nname = \"project-rule\"\ntool = \"bash\"\naction = \"allow\"\n",
        )
        .unwrap();

        let rules = Config::load_from(&[user, project])
            .unwrap()
            .permissions
            .expect("rules")
            .rules;
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].name.as_deref(), Some("project-rule"));
    }

    /// A project that pins a different provider must not inherit the user
    /// config's key/endpoint/model — those belong to the user's provider, and a
    /// Kimi key sent to Z.AI is rejected as if it were wrong.
    #[test]
    fn project_provider_override_drops_the_user_credential() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.toml");
        let project = dir.path().join(".rusta.toml");
        std::fs::write(
            &user,
            "provider = \"kimi\"\nbilling = \"subscription\"\napi_key = \"sk-kimi\"\nmodel = \"kimi-for-coding\"\n",
        )
        .unwrap();
        std::fs::write(&project, "provider = \"glm\"\n").unwrap();

        let config = Config::load_from(&[user, project]).unwrap();
        assert_eq!(config.provider.as_deref(), Some("glm"));
        assert_eq!(
            config.api_key, None,
            "the Kimi key must not leak to the GLM provider"
        );
        assert_eq!(config.model, None, "the Kimi model must not carry across");
    }

    /// `zai` is an alias for `glm`, so overriding one with the other is not a
    /// provider change and the key survives.
    #[test]
    fn zai_alias_is_not_a_provider_change() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.toml");
        let project = dir.path().join(".rusta.toml");
        std::fs::write(&user, "provider = \"glm\"\napi_key = \"sk-glm\"\n").unwrap();
        std::fs::write(&project, "provider = \"zai\"\n").unwrap();

        let config = Config::load_from(&[user, project]).unwrap();
        assert_eq!(config.provider.as_deref(), Some("zai"));
        assert_eq!(
            config.api_key.as_deref(),
            Some("sk-glm"),
            "the GLM key must survive the zai alias"
        );
    }

    /// Only the project layer goes through the gate, which sees the layers
    /// merged below it; what the gate returns is what applies.
    #[test]
    fn the_project_layer_goes_through_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.toml");
        let project = dir.path().join(PROJECT_FILE);
        let explicit = dir.path().join("explicit.toml");
        std::fs::write(&user, "model = \"user-model\"\n").unwrap();
        std::fs::write(&project, "model = \"project-model\"\nmax_turns = 7\n").unwrap();
        std::fs::write(&explicit, "temperature = 0.5\n").unwrap();

        let mut seen = Vec::new();
        let config = Config::load_sources(
            &[
                (Source::User, user),
                (Source::Project, project.clone()),
                (Source::Explicit, explicit),
            ],
            |lower, path, mut layer| {
                seen.push((lower.model.clone(), path.to_path_buf()));
                layer.model = None;
                Ok(layer)
            },
        )
        .unwrap();

        assert_eq!(seen, [(Some("user-model".to_string()), project)]);
        assert_eq!(
            config.model.as_deref(),
            Some("user-model"),
            "the gate's cut"
        );
        assert_eq!(config.max_turns, Some(7), "the rest of the layer applies");
        assert_eq!(config.temperature, Some(0.5), "later layers still apply");
    }

    /// A malformed file is an error, not a silent fallback to the layer below.
    #[test]
    fn an_unparseable_layer_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "this is not toml").unwrap();
        assert!(Config::load_from(&[bad]).is_err());
    }

    #[test]
    fn test_load_from_missing_files_returns_default() {
        let paths = vec![PathBuf::from("/does/not/exist.toml")];
        let config = Config::load_from(&paths).unwrap();
        assert!(config.provider.is_none());
    }

    #[test]
    fn test_load_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"provider = "glm"
model = "glm-5.1"
api_key = "test-key"
"#,
        )
        .unwrap();

        let config = Config::load_from(&[path]).unwrap();
        assert_eq!(config.provider.as_deref(), Some("glm"));
        assert_eq!(config.model.as_deref(), Some("glm-5.1"));
        assert_eq!(config.api_key.as_deref(), Some("test-key"));
    }

    /// The documented configuration example must deserialize into `Config`.
    ///
    /// Guards against the example drifting from the struct: a renamed field, a
    /// changed type, or an invalid `permissions.rules` action all fail here.
    #[test]
    fn documented_config_example_deserializes() {
        const DOC: &str = include_str!("../../../docs/configuration.md");

        let block = DOC
            .split("```toml")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("docs/configuration.md must contain a ```toml example");

        let config: Config = toml::from_str(block).unwrap_or_else(|e| {
            panic!("docs/configuration.md toml example does not deserialize: {e}")
        });

        assert_eq!(config.provider.as_deref(), Some("glm"));
        assert_eq!(config.mcp_servers.len(), 3);
        let hosted = config
            .mcp_servers
            .iter()
            .find(|s| s.oauth.is_some())
            .expect("the example must show an OAuth server");
        let oauth = hosted.oauth.as_ref().unwrap();
        assert_eq!(oauth.client_id, "my-client");
        assert!(
            oauth.auth_url.is_empty() && oauth.token_url.is_empty(),
            "the example relies on discovery"
        );
        let spawned = config
            .mcp_servers
            .iter()
            .find(|s| s.command.is_some())
            .expect("the example must show a spawned server");
        assert_eq!(
            spawned.cwd.as_deref(),
            Some(std::path::Path::new("backend")),
            "the documented `cwd` must be a real field, not prose"
        );
        let rules = &config.permissions.expect("permissions block").rules;
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].tool.as_deref(), Some("read_file"));
    }

    /// `prompt_cache_ttl` takes Moonshot's two values, and a project layer
    /// overrides the user's.
    #[test]
    fn the_prompt_cache_lifetime_is_read_and_layered() {
        use lm_bridge::provider::PromptCacheTtl;
        let mut lower: Config = toml::from_str("prompt_cache_ttl = \"1h\"").unwrap();
        assert_eq!(lower.prompt_cache_ttl, Some(PromptCacheTtl::OneHour));
        lower.overlay(toml::from_str("prompt_cache_ttl = \"5m\"").unwrap());
        assert_eq!(lower.prompt_cache_ttl, Some(PromptCacheTtl::FiveMinutes));
        assert!(toml::from_str::<Config>("prompt_cache_ttl = \"2h\"").is_err());
    }

    /// `api_key` and `api_key_env` exclude each other: a layer naming one
    /// replaces the other, and one file naming both is an error.
    #[test]
    fn api_key_and_api_key_env_exclude_each_other() {
        let mut lower: Config = toml::from_str("api_key = \"sk-file\"").unwrap();
        lower.overlay(toml::from_str("api_key_env = \"WORK_KEY\"").unwrap());
        assert_eq!(lower.api_key, None);
        assert_eq!(lower.api_key_env.as_deref(), Some("WORK_KEY"));
        lower.overlay(toml::from_str("api_key = \"sk-project\"").unwrap());
        assert_eq!(lower.api_key.as_deref(), Some("sk-project"));
        assert_eq!(lower.api_key_env, None);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "api_key = \"a\"\napi_key_env = \"B\"\n").unwrap();
        let err = Config::load_from(&[path]).unwrap_err();
        assert!(
            err.to_string().contains("both api_key and api_key_env"),
            "{err}"
        );
    }

    /// Custom headers merge across layers, the higher layer winning per
    /// name, and go with the provider they were written for.
    #[test]
    fn custom_headers_layer_per_name() {
        let mut lower: Config = toml::from_str(
            "provider = \"kimi\"\n[custom_headers]\nX-Route = \"blue\"\nX-Team = \"core\"\n",
        )
        .unwrap();
        lower.overlay(toml::from_str("[custom_headers]\nX-Route = \"green\"\n").unwrap());
        assert_eq!(lower.custom_headers["X-Route"], "green");
        assert_eq!(lower.custom_headers["X-Team"], "core");

        lower.overlay(toml::from_str("provider = \"glm\"").unwrap());
        assert!(
            lower.custom_headers.is_empty(),
            "the old provider's headers stay behind"
        );
    }

    /// **Every scalar setting must layer.** A setting that did not would be
    /// silently discarded in favour of the lower layer's value — a project
    /// `.rusta.toml` raising `max_turns` for a long autonomous task, say — with
    /// nothing to say it had been ignored. Setting each to a distinct value in
    /// the higher layer catches the next field added to `Config` here, rather
    /// than by someone wondering why their setting does nothing.
    #[test]
    fn every_scalar_setting_is_overlaid_by_the_higher_layer() {
        let mut lower: Config = toml::from_str(
            r#"
provider = "kimi"
model = "kimi-for-coding"
timeout = 1
reasoning_effort = "low"
reasoning_key = "reasoning_content"
max_tokens = 1
max_turns = 1
max_attempts_per_step = 1
max_subagent_depth = 1
idle_timeout = 1
subagent_timeout_ms = 1
swarm_timeout_ms = 1
swarm_max_concurrency = 1
repeat_breaker = true
auto_session_title = true
dangerous_command_guard = true
headless_background_mode = "drain"
headless_wait_ceiling_s = 1
headless_max_turns = 1
temperature = 0.1
background_max_running_tasks = 1
"#,
        )
        .expect("lower must parse");
        let higher: Config = toml::from_str(
            r#"
provider = "glm"
model = "glm-5.3"
timeout = 9
reasoning_effort = "max"
reasoning_key = "reasoning"
max_tokens = 9
max_turns = 9
max_attempts_per_step = 9
max_subagent_depth = 9
idle_timeout = 9
subagent_timeout_ms = 9
swarm_timeout_ms = 9
swarm_max_concurrency = 9
repeat_breaker = false
auto_session_title = false
dangerous_command_guard = false
headless_background_mode = "exit"
headless_wait_ceiling_s = 9
headless_max_turns = 9
temperature = 0.9
background_max_running_tasks = 9
"#,
        )
        .expect("higher must parse");

        lower.overlay(higher);

        assert_eq!(lower.provider.as_deref(), Some("glm"));
        assert_eq!(lower.model.as_deref(), Some("glm-5.3"));
        assert_eq!(lower.timeout, Some(9));
        assert_eq!(lower.reasoning_effort.as_deref(), Some("max"));
        assert_eq!(lower.reasoning_key.as_deref(), Some("reasoning"));
        assert_eq!(lower.max_tokens, Some(9));
        assert_eq!(lower.max_turns, Some(9), "max_turns must layer");
        assert_eq!(lower.max_attempts_per_step, Some(9));
        assert_eq!(lower.max_subagent_depth, Some(9));
        assert_eq!(lower.idle_timeout, Some(9));
        assert_eq!(lower.subagent_timeout_ms, Some(9));
        assert_eq!(lower.swarm_timeout_ms, Some(9));
        assert_eq!(lower.swarm_max_concurrency, Some(9));
        assert_eq!(lower.repeat_breaker, Some(false));
        assert_eq!(lower.auto_session_title, Some(false));
        assert_eq!(lower.dangerous_command_guard, Some(false));
        assert_eq!(
            lower.headless_background_mode,
            Some(crate::cli::HeadlessBackgroundMode::Exit)
        );
        assert_eq!(lower.headless_wait_ceiling_s, Some(9));
        assert_eq!(lower.headless_max_turns, Some(9));
        assert_eq!(lower.temperature, Some(0.9));
        assert_eq!(lower.background_max_running_tasks, Some(9));
    }

    /// **Per-server > global > built-in.** A server that names its own budget
    /// knows something the global setting does not, so it is never overwritten.
    #[test]
    fn mcp_timeouts_resolve_per_server_over_global_over_builtin() {
        let mut config: Config = toml::from_str(
            r#"
[mcp]
startup_timeout_ms = 45000
tool_timeout_ms = 90000

[[mcp_servers]]
name = "opinionated"
command = "/opt/slow"
startup_timeout_ms = 120000

[[mcp_servers]]
name = "quiet"
command = "/opt/quiet"
"#,
        )
        .expect("must parse");
        config.apply_mcp_timeout_defaults();

        assert_eq!(
            config.mcp_servers[0].startup_timeout().as_millis(),
            120_000,
            "a per-server value must survive the global default"
        );
        assert_eq!(
            config.mcp_servers[0].tool_timeout().as_millis(),
            90_000,
            "...while the field it left unset still takes the global"
        );
        assert_eq!(config.mcp_servers[1].startup_timeout().as_millis(), 45_000);
        assert_eq!(config.mcp_servers[1].tool_timeout().as_millis(), 90_000);
    }

    /// With nothing configured, the built-in budgets stand exactly as before.
    #[test]
    fn mcp_timeouts_are_unchanged_when_nothing_is_configured() {
        let mut config: Config =
            toml::from_str("[[mcp_servers]]\nname = \"plain\"\ncommand = \"/opt/plain\"\n")
                .expect("must parse");
        config.apply_mcp_timeout_defaults();
        assert_eq!(
            config.mcp_servers[0].startup_timeout().as_millis(),
            u128::from(mcp_client::config::DEFAULT_STARTUP_TIMEOUT_MS)
        );
        assert_eq!(
            config.mcp_servers[0].tool_timeout().as_millis(),
            u128::from(mcp_client::config::DEFAULT_TOOL_TIMEOUT_MS)
        );
    }

    /// **Per-server > global > size rule.** The global `default_deferred`
    /// defers even a one-tool server; a per-server `deferred` always wins over
    /// it, in both directions; with the global unset the more-than-ten-tools
    /// rule applies.
    #[test]
    fn mcp_default_deferred_resolves_per_server_over_global_over_size() {
        use mcp_client::config::DEFER_TOOL_THRESHOLD;
        let config: Config = toml::from_str(
            r#"
[mcp]
default_deferred = true

[[mcp_servers]]
name = "quiet"
command = "/opt/quiet"

[[mcp_servers]]
name = "inline-anyway"
command = "/opt/inline"
deferred = false

[[mcp_servers]]
name = "deferred-anyway"
command = "/opt/deferred"
deferred = true
"#,
        )
        .expect("must parse");
        let global = config.mcp.default_deferred;
        let server = |name: &str| {
            config
                .mcp_servers
                .iter()
                .find(|s| s.name == name)
                .expect("server is in the fixture")
        };

        assert_eq!(global, Some(true));
        assert!(
            server("quiet").defers_tools(1, global),
            "global on defers even a one-tool server"
        );
        assert!(
            !server("inline-anyway").defers_tools(1, global),
            "per-server deferred = false opts out under a global on"
        );
        assert!(
            server("deferred-anyway").defers_tools(1, global),
            "per-server deferred = true defers under a global on"
        );

        // Global off: even a large server stays inline.
        assert!(!server("quiet").defers_tools(1, Some(false)));
        assert!(
            !server("quiet").defers_tools(DEFER_TOOL_THRESHOLD + 1, Some(false)),
            "global off overrides the size rule too"
        );
        assert!(
            server("deferred-anyway").defers_tools(1, Some(false)),
            "per-server deferred = true wins over a global off"
        );
        // Global unset: the size rule stands exactly as before.
        assert!(!server("quiet").defers_tools(DEFER_TOOL_THRESHOLD, None));
        assert!(server("quiet").defers_tools(DEFER_TOOL_THRESHOLD + 1, None));
    }

    /// `RUSTA_MCP_DEFAULT_DEFERRED` overrides the config file's
    /// `[mcp] default_deferred`, and a malformed value is ignored rather than
    /// fatal — on `env_millis`'s terms.
    #[test]
    fn mcp_default_deferred_env_override_wins() {
        let deferred = |value: &'static str| {
            move |var: &str| (var == "RUSTA_MCP_DEFAULT_DEFERRED").then(|| value.to_string())
        };
        let mut config: Config = toml::from_str(
            "[mcp]\ndefault_deferred = false\n[[mcp_servers]]\nname = \"a\"\ncommand = \"/a\"\n",
        )
        .expect("must parse");

        config.apply_mcp_env_overrides_with(deferred("true"));
        assert_eq!(
            config.mcp.default_deferred,
            Some(true),
            "env wins over file"
        );

        config.apply_mcp_env_overrides_with(deferred("garbage"));
        assert_eq!(
            config.mcp.default_deferred,
            Some(true),
            "a malformed value is ignored, not fatal"
        );
    }

    /// The `[mcp]` section layers like every other key: the higher-precedence
    /// file's `default_deferred` wins where it sets one.
    #[test]
    fn mcp_default_deferred_layers_across_config_files() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.toml");
        let project = dir.path().join(".rusta.toml");
        std::fs::write(&user, "[mcp]\ndefault_deferred = true\n").unwrap();
        std::fs::write(&project, "[mcp]\ntool_timeout_ms = 3000\n").unwrap();

        let config = Config::load_from(&[user, project]).unwrap();
        assert_eq!(
            config.mcp.default_deferred,
            Some(true),
            "the lower layer's value survives a higher layer that says nothing"
        );
        assert_eq!(config.mcp.tool_timeout_ms, Some(3000));

        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.toml");
        let project = dir.path().join(".rusta.toml");
        std::fs::write(&user, "[mcp]\ndefault_deferred = true\n").unwrap();
        std::fs::write(&project, "[mcp]\ndefault_deferred = false\n").unwrap();

        let config = Config::load_from(&[user, project]).unwrap();
        assert_eq!(
            config.mcp.default_deferred,
            Some(false),
            "the higher layer wins where both set the knob"
        );
    }

    /// Hook rules from both layers accumulate; each keeps its own fields.
    #[test]
    fn hooks_from_both_layers_accumulate() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.toml");
        let project = dir.path().join(".rusta.toml");
        std::fs::write(
            &user,
            "[[hooks]]\nevent = \"Stop\"\ncommand = \"echo user\"\n",
        )
        .unwrap();
        std::fs::write(
            &project,
            "[[hooks]]\nevent = \"PreToolUse\"\nmatcher = \"Bash\"\ncommand = \"echo project\"\ntimeout = 5\n",
        )
        .unwrap();

        let config = Config::load_from(&[user, project]).unwrap();
        assert_eq!(config.hooks.len(), 2);
        assert_eq!(
            config.hooks[0].event,
            agent_core::user_hooks::UserHookEvent::Stop
        );
        assert_eq!(
            config.hooks[1].event,
            agent_core::user_hooks::UserHookEvent::PreToolUse
        );
        assert_eq!(config.hooks[1].timeout, Some(5));
    }

    /// A rule with an unknown field, an unknown event, a bad regex or an out-
    /// of-range timeout fails the config load, with the file named.
    #[test]
    fn invalid_hook_rules_fail_the_config_load() {
        for contents in [
            "[[hooks]]\nevent = \"Stop\"\ncommand = \"true\"\nextra = 1\n",
            "[[hooks]]\nevent = \"Nope\"\ncommand = \"true\"\n",
            "[[hooks]]\nevent = \"Stop\"\ncommand = \"true\"\nmatcher = \"(\"\n",
            "[[hooks]]\nevent = \"Stop\"\ncommand = \"true\"\ntimeout = 601\n",
            "[[hooks]]\nevent = \"Stop\"\ncommand = \"\"\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            std::fs::write(&path, contents).unwrap();
            let err = Config::load_from(&[path]).unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("hooks")
                    || message.contains("unknown field")
                    || message.contains("unknown variant"),
                "{message}"
            );
        }
    }
}
