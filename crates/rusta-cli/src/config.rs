//! `rusta.toml` configuration — ADR §7.
//!
//! Discovery: `rusta.toml` in the cwd, then every parent, then `~/.rusta/`.
//! The first file found wins; every section has a built-in default, so an
//! absent file is a valid configuration (the §7 defaults). CLI flags
//! (`--backend`, `--model`, `--base-url`) override file values field by
//! field in [`Config::http`] — flag over file over default (§6.2).

use std::path::{Path, PathBuf};

use rusta_llm::HttpConfig;
use rusta_validate::ValidateConfig;
use serde::Deserialize;

/// The whole `rusta.toml` — every section optional with §7 defaults.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Backend selection and endpoint.
    pub backend: BackendSection,
    /// Sampling and window settings.
    pub model: ModelSection,
    /// Agent behavior.
    pub agent: AgentSection,
    /// Repo-map budget.
    pub repomap: RepomapSection,
    /// Validators (§6.7) — reused verbatim from `rusta-validate`.
    pub validate: ValidateConfig,
    /// Shell policy (§6.12).
    pub shell: ShellSection,
}

/// `[backend]` — §7 schema verbatim.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BackendSection {
    /// `"http"` (default) or `"embedded"`.
    pub kind: String,
    /// Full API root including `/v1` (§6.2).
    pub base_url: Option<String>,
    /// Env var holding the API key; read once, never stored.
    pub api_key_env: Option<String>,
    /// `[backend.embedded]` — used only when `kind = "embedded"`.
    pub embedded: EmbeddedSection,
}

impl Default for BackendSection {
    fn default() -> Self {
        Self {
            kind: "http".to_owned(),
            base_url: None,
            api_key_env: None,
            embedded: EmbeddedSection::default(),
        }
    }
}

/// `[backend.embedded]` — parsed in every build (the schema is stable); used
/// only behind the `embedded` feature at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EmbeddedSection {
    /// GGUF path (`~` expanded).
    pub model_path: Option<String>,
    /// Context size override (0 = GGUF metadata).
    pub ctx_size: u32,
    /// GPU layers (0 = CPU, 999 = all).
    pub gpu_layers: u32,
}

impl Default for EmbeddedSection {
    fn default() -> Self {
        Self {
            model_path: None,
            ctx_size: 0,
            gpu_layers: 999,
        }
    }
}

/// `[model]` — §7 schema verbatim.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelSection {
    /// Model name sent in the request body.
    pub name: Option<String>,
    /// Context window (HTTP backend; embedded reads GGUF metadata).
    pub context_window: u32,
    /// Max completion tokens.
    pub max_tokens: u32,
    /// Sampling temperature.
    pub temperature: f32,
}

impl Default for ModelSection {
    fn default() -> Self {
        Self {
            name: None,
            context_window: 32_768,
            max_tokens: 4_096,
            temperature: 0.2,
        }
    }
}

/// `[agent]` — §7 schema verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentSection {
    /// Model turns per user request (§6.1 hard cap).
    pub max_turns: u32,
    /// Approve the plan gate + shell without prompting (§6.12 `/auto`).
    pub auto_approve: bool,
}

impl Default for AgentSection {
    fn default() -> Self {
        Self {
            max_turns: 16,
            auto_approve: false,
        }
    }
}

/// `[repomap]` — §7 schema verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RepomapSection {
    /// Repo-map token budget; 0 disables the map (§6.5).
    pub max_tokens: u32,
}

impl Default for RepomapSection {
    fn default() -> Self {
        Self { max_tokens: 1_024 }
    }
}

/// `[shell]` — §7 schema verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShellSection {
    /// Execution timeout.
    pub timeout_secs: u64,
    /// Extra allow-listed command prefixes (skip the approval prompt).
    pub allow: Vec<String>,
    /// Extra deny regexes extending the §6.12 table.
    pub deny: Vec<String>,
    /// Environment variable *names* the `shell` tool forwards on top of
    /// `PATH`/`HOME`/`LANG` — §6.12's "minimal environment (… + config
    /// allow-list)". `shell` is the only subprocess Rusta runs with a
    /// cleared environment; validators inherit the parent's already. Values
    /// always come from the parent process, never from this file.
    pub env: Vec<String>,
}

impl Default for ShellSection {
    fn default() -> Self {
        Self {
            timeout_secs: 60,
            allow: Vec::new(),
            deny: Vec::new(),
            env: Vec::new(),
        }
    }
}

/// CLI overrides gathered from flags — each `None` means "use the file".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    /// `--backend http|embedded`.
    pub backend_kind: Option<String>,
    /// `--model <name>`.
    pub model: Option<String>,
    /// `--base-url <url>`.
    pub base_url: Option<String>,
}

impl Config {
    /// Parses `text` as a full `rusta.toml`, rejecting misuse the agent could
    /// not act on anyway (§6.11: actionable messages at the CLI boundary).
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Config = toml::from_str(text).map_err(|e| format!("rusta.toml: {e}"))?;
        match config.backend.kind.as_str() {
            "http" | "embedded" => {}
            other => {
                return Err(format!(
                    "rusta.toml [backend]: kind must be \"http\" or \"embedded\" (got {other:?})"
                ));
            }
        }
        if config.agent.max_turns == 0 {
            return Err("rusta.toml [agent]: max_turns must be at least 1".to_owned());
        }
        config
            .validate
            .check()
            .map_err(|e| format!("rusta.toml [validate]: {e}"))?;
        Ok(config)
    }

    /// Loads the file at `path`; a missing or unreadable file is a loud,
    /// actionable error (§6.11) — the defaults come from `Config::default`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Resolves the `[backend]`/`[model]` sections plus CLI overrides into an
    /// [`HttpConfig`] (§6.2 precedence: flag over file over default).
    pub fn http(&self, overrides: &Overrides) -> HttpConfig {
        let defaults = HttpConfig::default();
        HttpConfig {
            base_url: overrides
                .base_url
                .clone()
                .or_else(|| self.backend.base_url.clone())
                .unwrap_or(defaults.base_url),
            model: overrides
                .model
                .clone()
                .or_else(|| self.model.name.clone())
                .unwrap_or(defaults.model),
            context_window: self.model.context_window,
            max_tokens: self.model.max_tokens,
            temperature: self.model.temperature,
            api_key_env: self.backend.api_key_env.clone(),
        }
    }

    /// The effective backend kind, with the CLI override applied.
    pub fn backend_kind(&self, overrides: &Overrides) -> String {
        overrides
            .backend_kind
            .clone()
            .unwrap_or_else(|| self.backend.kind.clone())
    }

    /// One-line summary for the `SessionStart` event (§6.10) — no secrets.
    pub fn summary(&self) -> String {
        format!(
            "backend={} model_ctx={} max_turns={} auto_approve={} validators={} shell_timeout={}s",
            self.backend.kind,
            self.model.context_window,
            self.agent.max_turns,
            self.agent.auto_approve,
            self.validate.commands.len(),
            self.shell.timeout_secs,
        )
    }
}

/// Searches for `rusta.toml` starting at `dir`, then upward through the
/// parents, then `~/.rusta/rusta.toml` (§7). First hit wins.
pub fn discover(dir: &Path) -> Option<PathBuf> {
    let mut current = Some(dir);
    while let Some(dir) = current {
        let candidate = dir.join("rusta.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    home_dir()
        .map(|home| home.join(".rusta").join("rusta.toml"))
        .filter(|p| p.is_file())
}

/// The user's home directory (`$HOME`, falling back to `$USERPROFILE`).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Session log path under `~/.rusta/sessions/` — `<slug>-<UTC date>.jsonl`
/// (§6.10). The slug is the repo directory name, sanitized to `[a-z0-9-]`.
pub fn session_path(root: &Path) -> Option<PathBuf> {
    let home = home_dir()?;
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "session".to_owned());
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_owned();
    let slug = if slug.is_empty() {
        "session".to_owned()
    } else {
        slug
    };
    Some(
        home.join(".rusta")
            .join("sessions")
            .join(format!("{slug}-{}.jsonl", utc_date())),
    )
}

/// The UTC date `YYYY-MM-DD` for the current wall clock. Std-only: days since
/// epoch converted with Howard Hinnant's `civil_from_days` algorithm.
pub fn utc_date() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Converts days since 1970-01-01 to a (year, month, day) civil date.
/// Proleptic Gregorian (Howard Hinnant, "chrono-Compatible Date Algorithms").
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { y + 1 } else { y }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_plan_section_seven() {
        let config = Config::default();
        assert_eq!(config.backend.kind, "http");
        assert_eq!(config.agent.max_turns, 16);
        assert!(!config.agent.auto_approve);
        assert_eq!(config.repomap.max_tokens, 1_024);
        assert_eq!(config.shell.timeout_secs, 60);
        assert!(config.validate.commands.is_empty());
        assert_eq!(config.model.context_window, 32_768);
        assert_eq!(config.model.max_tokens, 4_096);
    }

    #[test]
    fn parses_full_file_and_builds_http_config_with_precedence() {
        let config = Config::parse(
            r#"
[backend]
kind = "http"
base_url = "http://127.0.0.1:9000/v1"
api_key_env = "RUSTA_API_KEY"

[model]
name = "qwen3-coder-30b-a3b"
context_window = 16384

[validate]
commands = ["cargo check --workspace"]
timeout_secs = 120

[shell]
timeout_secs = 30
allow = ["cargo"]
"#,
        )
        .expect("valid config");

        let http = config.http(&Overrides::default());
        assert_eq!(http.base_url, "http://127.0.0.1:9000/v1");
        assert_eq!(http.model, "qwen3-coder-30b-a3b");
        assert_eq!(http.context_window, 16_384);
        assert_eq!(http.api_key_env.as_deref(), Some("RUSTA_API_KEY"));

        // CLI flags override the file (§6.2 precedence).
        let overridden = config.http(&Overrides {
            model: Some("gpt-oss-20b".to_owned()),
            base_url: Some("http://127.0.0.1:1234/v1".to_owned()),
            backend_kind: None,
        });
        assert_eq!(overridden.model, "gpt-oss-20b");
        assert_eq!(overridden.base_url, "http://127.0.0.1:1234/v1");
        assert_eq!(
            config.backend_kind(&Overrides {
                backend_kind: Some("embedded".to_owned()),
                ..Overrides::default()
            }),
            "embedded"
        );
        assert_eq!(config.shell.timeout_secs, 30);
        assert_eq!(config.validate.commands, ["cargo check --workspace"]);
    }

    #[test]
    fn rejects_unknown_backend_kind_and_unknown_keys() {
        let err = Config::parse("[backend]\nkind = \"cloud\"\n").unwrap_err();
        assert!(err.contains("kind must be"), "{err}");
        let err = Config::parse("[nonsense]\nkey = 1\n").unwrap_err();
        assert!(err.contains("unknown"), "{err}");
        let err = Config::parse("[agent]\nmax_turns = 0\n").unwrap_err();
        assert!(err.contains("max_turns"), "{err}");
    }

    #[test]
    fn discovery_walks_parents_nearest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("a").join("b");
        std::fs::create_dir_all(&nested).expect("mkdir");
        std::fs::write(nested.join("rusta.toml"), "[agent]\nmax_turns = 4\n").expect("near");
        std::fs::write(dir.path().join("rusta.toml"), "[agent]\nmax_turns = 5\n").expect("far");
        assert_eq!(discover(&nested), Some(nested.join("rusta.toml")));
        assert_eq!(
            discover(&dir.path().join("a")),
            Some(dir.path().join("rusta.toml"))
        );
    }

    #[test]
    fn shipped_example_config_parses_cleanly() {
        // The repo's rusta.toml.example (§7) can never drift from the schema.
        let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../rusta.toml.example");
        let text = std::fs::read_to_string(example).unwrap_or_else(|e| panic!("{example}: {e}"));
        let config = Config::parse(&text).expect("example parses");
        assert_eq!(config.backend.kind, "http");
        assert_eq!(config.agent.max_turns, 16);
        assert_eq!(config.validate.commands.len(), 3);
        assert_eq!(config.backend.embedded.gpu_layers, 999);
    }

    #[test]
    fn utc_date_matches_known_epochs() {
        // Fixed reference days (no wall clock involved).
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29)); // leap day
        assert_eq!(civil_from_days(19_781), (2024, 2, 28));
        assert_eq!(utc_date().len(), 10, "YYYY-MM-DD shape");
    }

    #[test]
    fn session_slug_is_sanitized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let weird = dir.path().join("My Repo_2!");
        std::fs::create_dir_all(&weird).expect("mkdir");
        if let Some(path) = session_path(&weird) {
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            assert!(name.starts_with("my-repo-2-"), "{name}");
            assert!(name.ends_with(".jsonl"), "{name}");
        }
        // Without a HOME the path is simply unavailable — the caller falls
        // back to a repo-local session file.
    }
}
