use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::endpoints::{Billing, Region};
use clap_complete::Shell;

#[derive(Parser, Debug, Clone, Default)]
#[command(name = "rusta")]
#[command(version)]
#[command(about = "Lean Rust coding agent for Kimi, GLM and DeepSeek models")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Path to a configuration file.
    #[arg(long, env = "RUSTA_CONFIG")]
    pub config: Option<PathBuf>,

    /// Provider to use (kimi, glm, zai, deepseek, or openai).
    #[arg(long, env = "RUSTA_PROVIDER")]
    pub provider: Option<String>,

    /// Model identifier.
    #[arg(long, env = "RUSTA_MODEL")]
    pub model: Option<String>,

    /// Which quota the API key is billed against: `subscription` or `api`.
    ///
    /// A Coding Plan key and a pay-per-token key are not interchangeable — each
    /// is rejected by the other's endpoint. This picks the right door, and with
    /// it the right default model.
    #[arg(long, value_enum, env = "RUSTA_BILLING")]
    pub billing: Option<Billing>,

    /// Regional deployment: `global` or `china`.
    #[arg(long, value_enum, env = "RUSTA_REGION")]
    pub region: Option<Region>,

    /// API base URL. Overrides the endpoint implied by --billing/--region.
    #[arg(long, env = "RUSTA_BASE_URL")]
    pub base_url: Option<String>,

    /// API key.
    #[arg(long, env = "RUSTA_API_KEY")]
    pub api_key: Option<String>,

    /// Single prompt to run in headless mode.
    ///
    /// Given without a value (`rusta -p`), the prompt is read from standard
    /// input, so `cat task.md | rusta -p` works.
    #[arg(short, long, num_args = 0..=1, default_missing_value = "")]
    pub prompt: Option<String>,

    /// Read the headless prompt from a file. `-` means standard input.
    ///
    /// A prompt long enough to be worth keeping in a file is also long enough
    /// to hit the shell's argument limit, and quoting it into `-p` mangles
    /// newlines.
    #[arg(long, conflicts_with = "prompt")]
    pub prompt_file: Option<PathBuf>,

    /// The session's working directory: tools run here, and the project's
    /// config and instruction files are read from here.
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,

    /// Permission mode (default, accept_edits, bypass_permissions).
    #[arg(long, env = "RUSTA_PERMISSION_MODE")]
    pub permission_mode: Option<String>,

    /// Bypass all permission checks (dangerous).
    #[arg(long)]
    pub yes: bool,

    /// Emit structured JSON stream output.
    #[arg(long)]
    pub stream_json: bool,

    /// Start in plan mode: the model plans, every edit waits for your
    /// approval, and the plan waits for your review before plan mode ends.
    #[arg(long, env = "RUSTA_PLAN")]
    pub plan: bool,

    /// Start in swarm mode, in which the agent splits the work it is given
    /// across `agent_swarm`, as `/swarm` turns it on in the REPL. Only the
    /// user turns swarm mode on, so a `-p` run swarms only with this.
    #[arg(long, env = "RUSTA_SWARM")]
    pub swarm: bool,

    /// Path to conversation records (JSONL).
    #[arg(long, env = "RUSTA_RECORDS")]
    pub records: Option<std::path::PathBuf>,

    /// URL template for the built-in web_search tool; `{query}` is replaced.
    #[arg(long, env = "RUSTA_SEARCH_URL")]
    pub search_url: Option<String>,

    /// Additional allowed path outside cwd (can be specified multiple times).
    #[arg(long = "allow-path", env = "RUSTA_ALLOW_PATH", value_delimiter = ',')]
    pub allowed_paths: Vec<std::path::PathBuf>,

    /// Session identifier used for records and resume.
    #[arg(long, env = "RUSTA_SESSION")]
    pub session: Option<String>,

    /// This run's identity: the session id when one was given, otherwise a
    /// fresh ULID. Not a flag — set once at startup. It keys the provider's
    /// prompt cache and names this run's spill directory, so neither depends on
    /// `--session` being passed.
    #[arg(skip)]
    pub run_id: Option<String>,

    /// Resume the conversation from the records file.
    #[arg(long, env = "RUSTA_RESUME")]
    pub resume: bool,

    /// Cap on a whole model request, in seconds, response included.
    ///
    /// Unset means uncapped, which is usually right for streaming: a long
    /// answer near a model's output limit is slow because it is working. Set it
    /// only when a run must not exceed a wall-clock budget. Stalls are caught by
    /// `--idle-timeout`, not by this.
    #[arg(long, env = "RUSTA_TIMEOUT")]
    pub timeout: Option<u64>,

    /// How long a model request may go silent, in seconds, before it is
    /// abandoned. Defaults to 300.
    ///
    /// The clock resets on every chunk received, so this catches a stalled
    /// connection without penalising a long, healthy stream. `0` disables it.
    #[arg(long, env = "RUSTA_IDLE_TIMEOUT")]
    pub idle_timeout: Option<u64>,

    /// Reasoning effort: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`.
    ///
    /// Sent to models that have an effort ladder — GLM-5.2 and newer,
    /// DeepSeek's models, and the Kimi models whose catalogue lists efforts —
    /// mapped onto that model's own ladder rather than rejected (GLM-5.3, Kimi
    /// and DeepSeek take `low` / `high` / `max`). On Kimi and DeepSeek, `none`
    /// turns thinking off. A model without a ladder is not sent it.
    #[arg(long, env = "RUSTA_REASONING_EFFORT")]
    pub reasoning_effort: Option<String>,

    /// Preserved Thinking: keep reasoning content from earlier turns (Kimi / GLM).
    ///
    /// Bare `--thinking-keep` enables it; `--thinking-keep=false` disables it.
    /// When absent, `KIMI_MODEL_THINKING_KEEP`, then the config file, then the
    /// model's own default decide. Kimi-K2.6-and-newer default to enabled; GLM
    /// defers to the endpoint (Z.AI's Coding Plan endpoint preserves by default,
    /// the standard API endpoint does not). Models whose schema lacks the field
    /// never receive it.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    pub thinking_keep: Option<bool>,

    /// How long Kimi's platform keeps a written prompt-cache entry: `5m` or
    /// `1h`.
    ///
    /// Its K3 series bills each cache write once — at twice the price for
    /// `1h` — and every hit renews the entry. Unset, an interactive session
    /// asks for `1h`, since its turns wait on a person, often for longer than
    /// five minutes, and a headless run takes the platform's `5m`, its steps
    /// following each other within seconds.
    #[arg(long, env = "RUSTA_PROMPT_CACHE_TTL")]
    pub prompt_cache_ttl: Option<lm_bridge::provider::PromptCacheTtl>,

    /// Maximum number of tokens to generate.
    #[arg(long, env = "RUSTA_MAX_TOKENS")]
    pub max_tokens: Option<u32>,

    /// Maximum model round-trips (steps) the agent may take before stopping with
    /// "Max turns exceeded". Raise it for long autonomous tasks; defaults to 100.
    #[arg(long, env = "RUSTA_MAX_TURNS")]
    pub max_turns: Option<u32>,

    /// How deep sub-agent delegation may nest. `0` forbids it entirely.
    ///
    /// Defaults to 1: the session's agent delegates, and its sub-agents do the
    /// work themselves. `2` lets them delegate once more.
    #[arg(long, env = "RUSTA_MAX_SUBAGENT_DEPTH")]
    pub max_subagent_depth: Option<usize>,

    /// Maximum attempts for a single model call before the error is surfaced.
    ///
    /// Counts the first attempt, so `1` disables retrying. Defaults to 10, which
    /// rides out the transient 429s the managed endpoints shed under load.
    #[arg(long, env = "RUSTA_MAX_ATTEMPTS_PER_STEP")]
    pub max_attempts_per_step: Option<u32>,

    /// Retry failing model calls without limit instead of giving up after
    /// `--max-attempts-per-step` attempts.
    ///
    /// For unattended runs that should wait out an outage or a spent quota
    /// window rather than fail. Failures no retry can fix — a context overrun,
    /// a malformed request, a content-filter refusal — still end the call.
    #[arg(long, env = "RUSTA_INFINITE_RETRY")]
    pub infinite_retry: bool,

    /// Maximum number of concurrently running background tasks.
    #[arg(long, env = "RUSTA_BACKGROUND_MAX_RUNNING_TASKS")]
    pub background_max_running_tasks: Option<usize>,

    /// Budget for one tool result, in characters, before it is cut to a head and
    /// a tail and the full output saved to a file the model can read back.
    /// Defaults to 50000; `0` disables the cap.
    #[arg(long, env = "RUSTA_TOOL_OUTPUT_MAX_CHARS")]
    pub tool_output_max_chars: Option<usize>,

    /// The most characters one `read_file` call may return when it asks for a
    /// larger `max_chars`. Calls that do not ask get 100000 (or this, if
    /// lower). Defaults to 500000; `0` removes the ceiling.
    #[arg(long, env = "RUSTA_READ_MAX_CHARS")]
    pub read_max_chars: Option<usize>,

    /// Model requests one context compaction may make — retries after a
    /// transient error, and retries with less history after an empty, cut-off
    /// or oversized summary, all count. When they run out, the oldest messages
    /// are evicted instead. Defaults to 5.
    #[arg(long, env = "RUSTA_COMPACTION_MAX_ATTEMPTS")]
    pub compaction_max_attempts: Option<u32>,

    /// Raw-byte budget for one image `read_media` sends. Larger images are
    /// re-encoded and downsampled to fit. Defaults to 262144 (256 KiB); `0`
    /// sets no budget.
    #[arg(long, env = "RUSTA_IMAGE_READ_BYTE_BUDGET")]
    pub image_read_byte_budget: Option<usize>,

    /// Send video inline to a vision model on a generic OpenAI-compatible
    /// endpoint (`--provider openai`). Off by default, since nothing
    /// advertises video support; Kimi and GLM decide per model.
    #[arg(long, env = "RUSTA_INLINE_VIDEO")]
    pub inline_video: bool,

    /// How long a delegated sub-agent may run, in milliseconds, before it is
    /// cancelled. `0` removes the deadline. Defaults to two hours.
    #[arg(long, env = "RUSTA_SUBAGENT_TIMEOUT_MS")]
    pub subagent_timeout_ms: Option<u64>,

    /// How long each `agent_swarm` member may run, in milliseconds, before it
    /// is cancelled and reports `timed_out`. `0` removes the deadline.
    /// Defaults to two hours.
    #[arg(long, env = "RUSTA_SWARM_TIMEOUT_MS")]
    pub swarm_timeout_ms: Option<u64>,

    /// How many `agent_swarm` members run at once. Unset runs them all
    /// together.
    #[arg(long, env = "RUSTA_SWARM_MAX_CONCURRENCY", value_parser = clap::value_parser!(u64).range(1..))]
    pub swarm_max_concurrency: Option<u64>,

    /// The repeat breaker: a tool call repeated across steps earns reminders,
    /// and a long run of one call stops the turn with a handoff. On by
    /// default; `--repeat-breaker=false` (or `RUSTA_REPEAT_BREAKER=0`) turns
    /// both off. A duplicate within one step runs once either way.
    #[arg(
        long,
        env = "RUSTA_REPEAT_BREAKER",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub repeat_breaker: Option<bool>,

    /// Ask before dangerous shell commands (`rm -rf`, `shutdown`, `dd` to a
    /// device) in the default and accept-edits modes; bypass mode (`--yes`)
    /// never asks. On by default. `--dangerous-command-guard=false` (or
    /// `RUSTA_DANGEROUS_COMMAND_GUARD=0`) turns it off, and with it the
    /// built-in prompts before writing secrets (`.env`, keys) and before
    /// `git push`, `reset`, `rebase` and forced checkouts and cleans.
    #[arg(
        long,
        env = "RUSTA_DANGEROUS_COMMAND_GUARD",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub dangerous_command_guard: Option<bool>,

    /// Trust this folder's `.rusta.toml` for this run: apply its settings
    /// that reach beyond the project (MCP servers, endpoints and keys,
    /// permissions) without asking or remembering. `--trust-workspace=false`
    /// (or `RUSTA_TRUST_WORKSPACE=0`) leaves them out, even in a trusted
    /// folder. Unset: a trusted folder applies them, and an interactive
    /// session asks.
    #[arg(
        long,
        env = "RUSTA_TRUST_WORKSPACE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    pub trust_workspace: Option<bool>,

    /// Wire field this endpoint uses for reasoning content.
    ///
    /// `reasoning_content` (Kimi, GLM, DeepSeek), `reasoning` (GPT-OSS, current
    /// vLLM), or `reasoning_details`. Left unset, Rusta detects it from the
    /// endpoint's own responses.
    #[arg(long, env = "RUSTA_REASONING_KEY")]
    pub reasoning_key: Option<String>,

    /// Sampling temperature (0.0–2.0; provider-specific range).
    #[arg(long, env = "RUSTA_TEMPERATURE")]
    pub temperature: Option<f32>,

    /// Nucleus-sampling top-p value (0.0–1.0).
    #[arg(long, env = "RUSTA_TOP_P")]
    pub top_p: Option<f32>,

    /// Enable model reasoning/thinking mode (Kimi / GLM).
    #[arg(long, env = "RUSTA_THINKING")]
    pub thinking: bool,

    /// Profile to use for the main agent and subagent registry.
    #[arg(long, env = "RUSTA_PROFILE")]
    pub profile: Option<String>,

    /// Additional project context file(s) (e.g. AGENTS.md) to inject into the
    /// system prompt. Can be specified multiple times.
    #[arg(long = "project-context-file", value_delimiter = ',')]
    pub project_context_files: Vec<std::path::PathBuf>,

    /// Additional directories to include as context (listings + AGENTS.md).
    #[arg(long = "add-dir", value_delimiter = ',')]
    pub additional_dirs: Vec<std::path::PathBuf>,

    /// Token budget for the project instruction files (`AGENTS.md` and the
    /// like) in the system prompt. Defaults to 8192.
    #[arg(long, env = "RUSTA_PROJECT_CONTEXT_BUDGET_TOKENS")]
    pub project_context_budget_tokens: Option<u64>,

    /// What a headless (`-p`) run does about background work once its prompt
    /// is answered: `steer` (default) holds the turn for sub-agents and keeps
    /// going while goals, cron jobs or notifications are pending; `drain`
    /// waits for sub-agents to finish without another model turn; `exit`
    /// ends at once.
    #[arg(long, value_enum, env = "RUSTA_HEADLESS_BACKGROUND_MODE")]
    pub headless_background_mode: Option<HeadlessBackgroundMode>,

    /// Longest time, in seconds, a headless run keeps waiting (`drain`) or
    /// steering (`steer`) after its prompt. Unset: no ceiling.
    #[arg(long, env = "RUSTA_HEADLESS_WAIT_CEILING_S", value_parser = clap::value_parser!(u64).range(1..))]
    pub headless_wait_ceiling_s: Option<u64>,

    /// Most follow-up turns a `steer` headless run takes for background
    /// notifications. Defaults to 100000.
    #[arg(long, env = "RUSTA_HEADLESS_MAX_TURNS", value_parser = clap::value_parser!(u32).range(1..))]
    pub headless_max_turns: Option<u32>,

    /// The config's `api_key_env`: the variable holding the key.
    #[arg(skip)]
    pub api_key_env: Option<String>,

    /// The config's `custom_headers`.
    #[arg(skip)]
    pub custom_headers: std::collections::BTreeMap<String, String>,
}

/// `rusta sessions` subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum SessionsAction {
    /// List saved sessions, most recent first: those that ran in the working
    /// directory, unless `--all`.
    List {
        /// Every session, wherever it ran.
        #[arg(long)]
        all: bool,
        /// Sessions that ran in this directory instead of the working one.
        #[arg(long, conflicts_with = "all")]
        cwd: Option<PathBuf>,
        /// At most this many sessions.
        #[arg(long)]
        limit: Option<usize>,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

/// What a headless run does about background work once its prompt is
/// answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeadlessBackgroundMode {
    /// End the run at once.
    Exit,
    /// Wait for sub-agents to finish, without another model turn.
    Drain,
    /// Hold the turn for sub-agents, and keep taking turns while goals, cron
    /// jobs or their notifications are pending.
    #[default]
    Steer,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Print version information.
    Version,
    /// Set up a provider, plan, and API key, and save them to the config file.
    Init,
    /// Validate configuration and API keys.
    Doctor,
    /// Generate shell completion scripts.
    Completions {
        /// Target shell.
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Run the terminal user interface (requires `tui` feature).
    #[cfg(feature = "tui")]
    Tui,
    /// Saved sessions.
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// Copy a saved session into a new one, to continue it without changing
    /// the original.
    Fork {
        /// The session to copy. Omitted: the latest one that ran in `--cwd`.
        session: Option<String>,
        /// Directory whose latest session to fork when none is named.
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        /// Id for the new session; a fresh one when omitted.
        #[arg(long = "as", value_name = "ID")]
        id: Option<String>,
        /// Fork without asking.
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// Trust this folder for the settings in its `.rusta.toml` that reach
    /// beyond it (MCP servers, endpoints and keys, permissions), after showing
    /// them. Remembered until they change.
    Trust {
        /// Forget this folder's trust.
        #[arg(long, conflicts_with_all = ["list", "yes"])]
        revoke: bool,
        /// List the trusted folders.
        #[arg(long, conflicts_with = "yes")]
        list: bool,
        /// Trust without asking.
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
    /// Export a session to a ZIP archive.
    Export {
        /// Session identifier to export.
        #[arg(long, short)]
        session: String,
        /// Output path for the ZIP file.
        #[arg(long, short)]
        output: PathBuf,
    },
}

impl Cli {
    /// Mints this run's id: the session id when one was given, otherwise a
    /// fresh lowercase ULID. Called once at startup, before anything is derived
    /// from `self`.
    pub fn assign_run_id(&mut self) {
        self.run_id = Some(
            self.session
                .clone()
                .unwrap_or_else(|| ulid::Ulid::generate().to_string().to_lowercase()),
        );
    }

    /// The key this run's prompt cache is grouped under: the session id, or the
    /// run id minted at startup when no session was named.
    #[must_use]
    pub fn cache_key(&self) -> Option<String> {
        self.session.clone().or_else(|| self.run_id.clone())
    }

    /// The prompt-cache lifetime this run asks for: the configured one, else
    /// an hour in an interactive session and the provider's default in a
    /// headless run (see `--prompt-cache-ttl`).
    #[must_use]
    pub fn cache_ttl(&self) -> Option<lm_bridge::provider::PromptCacheTtl> {
        use lm_bridge::provider::PromptCacheTtl;
        self.prompt_cache_ttl
            .or_else(|| (!self.is_headless()).then_some(PromptCacheTtl::OneHour))
    }

    /// The headless prompt for this invocation, if there is one.
    ///
    /// Resolves the three ways of supplying it: `-p <text>`, `--prompt-file
    /// <path>`, and standard input (`-p` with no value, or `--prompt-file -`).
    /// Returns `Ok(None)` when no prompt was requested, which is the signal to
    /// start an interactive session.
    ///
    /// Reading from a terminal is refused rather than left to block: `rusta
    /// -p` typed by hand would otherwise hang with no output and look like the
    /// model was thinking.
    /// Whether this is a headless run: a prompt from `-p` or `--prompt-file`.
    pub fn is_headless(&self) -> bool {
        self.prompt.is_some() || self.prompt_file.is_some()
    }

    /// Whether anyone can answer an approval in this run: always in an
    /// interactive session, and in a headless run unless it bypasses
    /// permissions (`--yes`), which runs unattended.
    pub fn can_ask(&self) -> bool {
        let bypass = self.yes
            || self
                .permission_mode
                .as_deref()
                .and_then(|mode| mode.parse::<agent_core::PermissionMode>().ok())
                == Some(agent_core::PermissionMode::BypassPermissions);
        !self.is_headless() || !bypass
    }

    pub fn resolve_prompt(&self) -> std::io::Result<Option<String>> {
        use std::io::{Read, Result};

        fn read_stdin(what: &str) -> Result<String> {
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "{what} reads the prompt from standard input, but stdin is a terminal. \
                         Pipe one in (`cat task.md | rusta -p`) or pass it inline (`-p \"…\"`)."
                    ),
                ));
            }
            let mut buffer = String::new();
            std::io::stdin().read_to_string(&mut buffer)?;
            Ok(buffer)
        }

        if let Some(path) = &self.prompt_file {
            let text = if path.as_os_str() == "-" {
                read_stdin("--prompt-file -")?
            } else {
                std::fs::read_to_string(path)?
            };
            return Ok(Some(text));
        }

        match self.prompt.as_deref() {
            None => Ok(None),
            // `-p` with no value: the prompt is on stdin.
            Some("") => read_stdin("-p with no value").map(Some),
            Some(text) => Ok(Some(text.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_background_settings_parse() {
        let cli = Cli::parse_from([
            "rusta",
            "--headless-background-mode",
            "drain",
            "--headless-wait-ceiling-s",
            "30",
            "--headless-max-turns",
            "5",
        ]);
        assert_eq!(
            cli.headless_background_mode,
            Some(HeadlessBackgroundMode::Drain)
        );
        assert_eq!(cli.headless_wait_ceiling_s, Some(30));
        assert_eq!(cli.headless_max_turns, Some(5));
        assert_eq!(
            HeadlessBackgroundMode::default(),
            HeadlessBackgroundMode::Steer
        );
        assert!(
            Cli::try_parse_from(["rusta", "--headless-max-turns", "0"]).is_err(),
            "a cap of zero turns is not a cap"
        );
        assert!(Cli::try_parse_from(["rusta", "--headless-background-mode", "wait"]).is_err());
    }

    /// `--trust-workspace` is a switch with an explicit off, and `rusta
    /// trust` cannot revoke and trust at once.
    #[test]
    fn trust_flags_parse() {
        assert_eq!(Cli::parse_from(["rusta"]).trust_workspace, None);
        assert_eq!(
            Cli::parse_from(["rusta", "--trust-workspace"]).trust_workspace,
            Some(true)
        );
        assert_eq!(
            Cli::parse_from(["rusta", "--trust-workspace=no"]).trust_workspace,
            Some(false)
        );
        assert!(matches!(
            Cli::parse_from(["rusta", "trust", "--revoke"]).command,
            Some(Commands::Trust {
                revoke: true,
                list: false,
                yes: false
            })
        ));
        assert!(Cli::try_parse_from(["rusta", "trust", "--revoke", "-y"]).is_err());
    }

    #[test]
    fn the_repeat_breaker_flag_reads_booleans() {
        assert_eq!(Cli::parse_from(["rusta"]).repeat_breaker, None);
        assert_eq!(
            Cli::parse_from(["rusta", "--repeat-breaker"]).repeat_breaker,
            Some(true)
        );
        assert_eq!(
            Cli::parse_from(["rusta", "--repeat-breaker=0"]).repeat_breaker,
            Some(false)
        );
        assert_eq!(
            Cli::parse_from(["rusta", "--repeat-breaker=off"]).repeat_breaker,
            Some(false)
        );
    }

    fn cli_for_prompt(args: &[&str]) -> Cli {
        let mut full = vec!["rusta"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    /// Every run has a prompt cache key: without `--session` it is a run id
    /// minted at startup, distinct per run; with one, it is the session id.
    #[test]
    fn every_run_has_a_prompt_cache_key() {
        let mut first = cli_for_prompt(&["-p", "hi"]);
        let mut second = cli_for_prompt(&["-p", "hi"]);
        assert_eq!(first.cache_key(), None, "minted at startup, not by clap");
        first.assign_run_id();
        second.assign_run_id();
        let key = first.cache_key().expect("a run id keys the cache");
        assert_eq!(key.len(), 26);
        assert_eq!(key, key.to_lowercase());
        assert_ne!(first.cache_key(), second.cache_key());

        let mut named = cli_for_prompt(&["--session", "work", "-p", "hi"]);
        named.assign_run_id();
        assert_eq!(named.cache_key().as_deref(), Some("work"));
        assert_eq!(named.run_id.as_deref(), Some("work"));
    }

    #[test]
    fn an_inline_prompt_is_used_verbatim() {
        assert_eq!(
            cli_for_prompt(&["-p", "do the thing"])
                .resolve_prompt()
                .unwrap(),
            Some("do the thing".to_string())
        );
    }

    /// No `-p` at all means an interactive session, not an empty prompt.
    #[test]
    fn no_prompt_flag_means_interactive() {
        assert_eq!(cli_for_prompt(&[]).resolve_prompt().unwrap(), None);
    }

    /// `-p` with no value asks for standard input, which is what makes
    /// `cat task.md | rusta -p` work.
    #[test]
    fn a_valueless_prompt_flag_requests_stdin() {
        assert_eq!(
            cli_for_prompt(&["-p"]).prompt.as_deref(),
            Some(""),
            "the empty value is the marker that stdin should be read"
        );
    }

    #[test]
    fn a_prompt_file_is_read_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.md");
        std::fs::write(&path, "from a file\nwith newlines\n").unwrap();
        assert_eq!(
            cli_for_prompt(&["--prompt-file", path.to_str().unwrap()])
                .resolve_prompt()
                .unwrap(),
            Some("from a file\nwith newlines\n".to_string()),
            "newlines must survive, which is half the point of a file"
        );
    }

    #[test]
    fn a_missing_prompt_file_is_an_error_not_an_empty_prompt() {
        assert!(
            cli_for_prompt(&["--prompt-file", "/nonexistent/task.md"])
                .resolve_prompt()
                .is_err()
        );
    }

    /// Two sources would be ambiguous, so clap rejects the combination.
    #[test]
    fn a_prompt_and_a_prompt_file_together_are_rejected() {
        let mut full = vec!["rusta", "-p", "inline", "--prompt-file", "x"];
        full.truncate(5);
        assert!(Cli::try_parse_from(full).is_err());
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("rusta").chain(args.iter().copied()))
    }

    /// An interactive session asks for an hour-long prompt cache, a headless
    /// run for the provider's default; a configured lifetime wins in both.
    #[test]
    fn the_prompt_cache_lifetime_follows_the_kind_of_run() {
        use lm_bridge::provider::PromptCacheTtl;
        assert_eq!(
            parse(&[]).unwrap().cache_ttl(),
            Some(PromptCacheTtl::OneHour)
        );
        assert_eq!(parse(&["-p", "hi"]).unwrap().cache_ttl(), None);
        assert_eq!(
            parse(&["-p", "hi", "--prompt-cache-ttl", "1h"])
                .unwrap()
                .cache_ttl(),
            Some(PromptCacheTtl::OneHour)
        );
        assert_eq!(
            parse(&["--prompt-cache-ttl", "5m"]).unwrap().cache_ttl(),
            Some(PromptCacheTtl::FiveMinutes)
        );
        assert!(parse(&["--prompt-cache-ttl", "2h"]).is_err());
    }

    /// `--thinking-keep` is tri-state: present-and-true, present-and-false, or
    /// absent (defer to env, then config, then the model's default).
    #[test]
    fn thinking_keep_is_tri_state() {
        assert_eq!(parse(&["version"]).unwrap().thinking_keep, None);
        assert_eq!(
            parse(&["--thinking-keep", "version"])
                .unwrap()
                .thinking_keep,
            Some(true),
            "the bare flag enables it"
        );
        assert_eq!(
            parse(&["--thinking-keep=true"]).unwrap().thinking_keep,
            Some(true)
        );
        assert_eq!(
            parse(&["--thinking-keep=false"]).unwrap().thinking_keep,
            Some(false),
            "there must be a way to switch off the Kimi default"
        );
    }

    /// `require_equals` keeps the optional value from eating the next argument:
    /// without it, `rusta --thinking-keep version` reads `version` as the
    /// flag's value and dies.
    #[test]
    fn thinking_keep_never_swallows_the_following_argument() {
        let cli = parse(&["--thinking-keep", "version"]).expect("must parse");
        assert_eq!(cli.thinking_keep, Some(true));
        assert!(matches!(cli.command, Some(Commands::Version)));

        assert!(
            parse(&["--thinking-keep", "true", "version"]).is_err(),
            "a space-separated value must be rejected, not silently consumed"
        );
    }

    #[test]
    fn thinking_keep_rejects_non_boolean_values() {
        assert!(parse(&["--thinking-keep=banana"]).is_err());
    }

    /// clap's own consistency check: catches conflicting/duplicate arg config.
    #[test]
    fn cli_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
