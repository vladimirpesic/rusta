//! Devscriptor MCP integration: discovery, auto-configuration, and system-prompt
//! guidance.
//!
//! Devscriptor exposes a 75-tool code-intelligence MCP server: search and
//! analysis over a persistent SQLite graph, a five-tool Context Graph, LSP-backed
//! navigation, six language-server lifecycle tools, and five architecture tools.
//! Those tools reach the model through progressive disclosure: only their names
//! and descriptions are announced, and the model must `select_tools` to load a
//! definition before calling it.
//!
//! Without guidance a model does not know that a `scan_codebase` must precede
//! entity search, that LSP tools need a language server resolved first, or that
//! the Context Graph is durable cross-session memory. This module supplies that
//! workflow as a system-prompt section, gated on a Devscriptor server actually
//! being configured and enabled.
//!
//! # Discovery
//!
//! A Devscriptor release archive ships exactly one executable, `devscriptor`, and
//! the MCP server is started with its `mcp` subcommand. [`discover`] therefore
//! looks for that single name and always launches it as `devscriptor mcp`.
//!
//! No separate `devscriptor-mcp` entry point is looked for: as a relative
//! symlink it would dangle when the binary is installed alone, and as a
//! `.cmd` shim on Windows `std::process::Command` cannot spawn it at all.

use std::path::{Path, PathBuf};

use mcp_client::config::{McpDefaults, McpServerConfig};

/// Environment variable pointing directly at a `devscriptor` executable.
/// Takes priority over every search path.
pub const BIN_ENV_VAR: &str = "DEVSCRIPTOR_BIN";

/// Environment variable naming the index the MCP server should open. Overrides
/// the project-local default.
pub const DB_ENV_VAR: &str = "DEVSCRIPTOR_DB";

/// File name Devscriptor uses for a project-local index — the one `devscriptor
/// scan` writes in the directory it was run from.
const DB_FILE_NAME: &str = "devscriptor.db";

/// Name Rusta gives the auto-configured server. The permission rules and the
/// `mcp__devscriptor__<tool>` prefix in the system prompt depend on it.
pub const SERVER_NAME: &str = "devscriptor";

/// The first start may load the embedding model from disk.
const STARTUP_TIMEOUT_MS: u64 = 60_000;

/// `scan_codebase`, a cold `semantic_search`, and the first LSP call (which
/// lazily boots a language server) routinely exceed the 60s default.
const TOOL_TIMEOUT_MS: u64 = 180_000;

/// How to spawn the Devscriptor MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevscriptorLaunch {
    /// Program to execute.
    pub command: String,
    /// Arguments, e.g. `["mcp"]`.
    pub args: Vec<String>,
}

impl DevscriptorLaunch {
    /// The only way to start the server: the binary, plus the `mcp` subcommand.
    fn cli(path: &Path) -> Self {
        Self {
            command: path.to_string_lossy().into_owned(),
            args: vec!["mcp".to_string()],
        }
    }
}

/// Executable file name to look for on a given platform.
fn cli_names(windows: bool) -> &'static [&'static str] {
    if windows {
        &["devscriptor.exe"]
    } else {
        &["devscriptor"]
    }
}

/// Directories to search, in priority order: `PATH` first, then the locations
/// `docs/INSTALL.md` tells users to install into on each supported platform.
///
/// Devscriptor ships binaries for five targets — Linux x86_64, Linux aarch64,
/// macOS aarch64, macOS x86_64, and Windows x86_64 — so the Unix list covers both
/// Linux and macOS, including both Homebrew prefixes (`/usr/local/bin` on Intel,
/// `/opt/homebrew/bin` on Apple Silicon).
fn search_dirs(path_var: Option<&str>, home: Option<&Path>, windows: bool) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    if let Some(path_var) = path_var {
        let separator = if windows { ';' } else { ':' };
        dirs.extend(
            path_var
                .split(separator)
                .filter(|entry| !entry.is_empty())
                .map(PathBuf::from),
        );
    }

    if windows {
        for var in ["LOCALAPPDATA", "ProgramFiles", "ProgramW6432"] {
            if let Ok(base) = std::env::var(var) {
                dirs.push(PathBuf::from(&base).join("Programs").join("devscriptor"));
                dirs.push(PathBuf::from(&base).join("devscriptor"));
            }
        }
        if let Some(home) = home {
            dirs.push(home.join(".devscriptor").join("bin"));
        }
    } else {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
        // Homebrew on Apple Silicon; the Intel prefix is `/usr/local/bin` above.
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        if let Some(home) = home {
            dirs.push(home.join(".local").join("bin"));
            dirs.push(home.join("bin"));
            dirs.push(home.join(".devscriptor").join("bin"));
        }
    }

    dirs
}

/// Find a Devscriptor launch spec among `dirs`.
///
/// `exists` reports whether a candidate file is present, so the search is
/// testable without touching the filesystem.
fn find_launch(
    dirs: &[PathBuf],
    windows: bool,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<DevscriptorLaunch> {
    for dir in dirs {
        for name in cli_names(windows) {
            let candidate = dir.join(name);
            if exists(&candidate) {
                return Some(DevscriptorLaunch::cli(&candidate));
            }
        }
    }
    None
}

/// Locate an installed Devscriptor MCP server on this machine.
///
/// Order: `DEVSCRIPTOR_BIN` → `PATH` → the platform's documented install
/// directories. Returns `None` when Devscriptor is not installed.
pub fn discover() -> Option<DevscriptorLaunch> {
    let windows = cfg!(windows);

    if let Ok(explicit) = std::env::var(BIN_ENV_VAR) {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            // Always `<path> mcp`. The standalone development binary ignores its
            // arguments, so this is correct for it too.
            return Some(DevscriptorLaunch::cli(&path));
        }
        tracing::warn!(
            "{BIN_ENV_VAR} points at {}, which is not a file; falling back to PATH",
            path.display()
        );
    }

    let path_var = std::env::var("PATH").ok();
    let home = dirs::home_dir();
    let dirs = search_dirs(path_var.as_deref(), home.as_deref(), windows);
    find_launch(&dirs, windows, &|p| p.is_file())
}

/// The index the auto-configured server should open, as an absolute path.
///
/// **Always absolute, and always passed explicitly.** `devscriptor mcp`
/// documents its default as the relative `devscriptor.db`, but a server spawned
/// without `--database` can open a *global* index under
/// `~/.local/share/devscriptor/` instead of the project's — and then the
/// agent's `semantic_search` and `get_statistics` answer from a different
/// database than the one `devscriptor scan` built in the workspace, one
/// accumulating every project the binary has touched. Naming the file leaves
/// nothing to resolve.
fn resolve_database(project_root: &Path) -> PathBuf {
    if let Some(explicit) = std::env::var_os(DB_ENV_VAR) {
        let path = PathBuf::from(explicit);
        if !path.as_os_str().is_empty() {
            return absolutize(&path, project_root);
        }
    }
    let root = absolutize(Path::new("."), project_root);
    find_index(&root, &|p| p.exists())
}

/// Pick the index for a session rooted at `root`: the one in the workspace if
/// it exists, otherwise an existing one further up the same repository.
///
/// The walk-up matters when a session is opened inside a monorepo package.
/// `devscriptor scan` writes its index where it was run — normally the repo
/// root — so a session in `packages/api` that insisted on a local file would
/// hand the model a *newly created, empty* index and silently discard the
/// repo-wide one it should have been reading.
///
/// The walk stops at the repository boundary: a directory holding `.git` is the
/// last one examined. Without that bound a stray `~/devscriptor.db` would
/// capture every session run anywhere under the home directory.
fn find_index(root: &Path, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    let local = root.join(DB_FILE_NAME);
    if exists(&local) {
        return local;
    }
    // `root` itself is already checked above; if it is the repo root, stop.
    if !exists(&root.join(".git")) {
        for ancestor in root.ancestors().skip(1) {
            let candidate = ancestor.join(DB_FILE_NAME);
            if exists(&candidate) {
                return candidate;
            }
            if exists(&ancestor.join(".git")) {
                break;
            }
        }
    }
    // Nothing indexed yet: the workspace is where a scan should land.
    local
}

/// The workspace the server should run in, as an absolute path.
fn absolute_root(project_root: &Path) -> PathBuf {
    absolutize(Path::new("."), project_root)
}

/// Resolve `path` against `base`, then against the process directory, so the
/// result never depends on the child's working directory.
///
/// `.` components are dropped rather than kept: `--cwd` defaults to `.`, and a
/// path like `/srv/app/./devscriptor.db` opens the right file but reads as a
/// mistake in the startup log.
fn absolutize(path: &Path, base: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        let relative = base.join(path);
        if relative.is_absolute() {
            relative
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&relative))
                .unwrap_or(relative)
        }
    };
    joined
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect()
}

/// Resolve any user-configured stdio server's working directory against the
/// session's workspace, so a relative `cwd` in a project config means what it
/// reads as.
///
/// A server that names no `cwd` is left alone: inheriting Rusta's own is the
/// documented behaviour, and silently relocating someone's server would be a
/// surprise. Only the Devscriptor server Rusta injects is placed
/// automatically.
pub fn normalize_server_cwds(servers: &mut [McpServerConfig], project_root: &Path) {
    let root = absolute_root(project_root);
    for server in servers {
        if let Some(cwd) = &server.cwd
            && !cwd.is_absolute()
        {
            server.cwd = Some(absolutize(cwd, &root));
        }
    }
}

/// Add a Devscriptor MCP server to `servers` when one is installed and the user
/// has not configured it themselves.
///
/// `project_root` is the workspace the session is running against; the server is
/// pointed at that directory's index. Returns the command that will be spawned,
/// for logging. An existing Devscriptor entry always wins — including a
/// `disabled = true` one, so a user can opt out.
///
/// Its timeouts are the global `[mcp]` ones in `defaults` where set, else
/// Devscriptor's own.
pub fn inject_devscriptor_server(
    servers: &mut Vec<McpServerConfig>,
    project_root: &Path,
    defaults: &McpDefaults,
) -> Option<String> {
    if servers.iter().any(is_devscriptor_server) {
        return None;
    }
    let launch = discover()?;
    Some(inject(servers, launch, project_root, defaults))
}

/// [`inject_devscriptor_server`], given the server to launch.
fn inject(
    servers: &mut Vec<McpServerConfig>,
    launch: DevscriptorLaunch,
    project_root: &Path,
    defaults: &McpDefaults,
) -> String {
    let root = absolute_root(project_root);

    let mut args = launch.args;
    args.push("--database".to_string());
    args.push(resolve_database(&root).to_string_lossy().into_owned());

    let described = format!("{} {}", launch.command, args.join(" "));

    servers.push(McpServerConfig {
        name: SERVER_NAME.to_string(),
        command: Some(launch.command),
        args,
        // Run the server *in* the workspace. `--database` pins the index, but
        // the server also resolves relative tool arguments, walks up to find a
        // project root, and spawns language servers that do the same — all of
        // which would otherwise happen in whatever directory Rusta was
        // launched from.
        cwd: Some(root),
        startup_timeout_ms: Some(defaults.startup_or(STARTUP_TIMEOUT_MS)),
        tool_timeout_ms: Some(defaults.tool_or(TOOL_TIMEOUT_MS)),
        ..Default::default()
    });

    described
}

/// Does this entry describe a Devscriptor server, enabled or not?
///
/// Matches on the server name, the spawn command, or any argument — the last of
/// which catches wrapper invocations such as `sh -c "devscriptor mcp"`.
fn is_devscriptor_server(server: &McpServerConfig) -> bool {
    if server.name.to_lowercase().contains("devscriptor") {
        return true;
    }
    server
        .command
        .as_deref()
        .map(|c| c.to_lowercase().contains("devscriptor"))
        .unwrap_or(false)
        || server
            .args
            .iter()
            .any(|a| a.to_lowercase().contains("devscriptor"))
}

/// Return the name of the configured, enabled Devscriptor MCP server, if any.
///
/// Deliberately the same test [`inject_devscriptor_server`] uses to decide that
/// a server is already configured. The two must agree: a config that suppresses
/// injection but is not recognised here yields a connected Devscriptor server
/// whose model was never told how to drive it — the worst of both.
pub fn configured_server(servers: &[McpServerConfig]) -> Option<String> {
    servers
        .iter()
        .find(|s| !s.disabled && is_devscriptor_server(s))
        .map(|s| s.name.clone())
}

/// How to find code, the same for every agent that has the tools.
const FINDING_CODE: &str = r#"## Finding code
- `semantic_search` — the default for a question in words ("where are shipping
  costs computed?"). It ranks the index by meaning and keywords together and
  returns each match with its file, its first and last line (1-indexed) and its
  first lines, each numbered as in the file: cite `file:line` from those rather
  than reading the file to count lines. Without embeddings it falls back to
  keywords and says so in its `message`.
- `search_functions` / `search_classes` / `search_entities` — lookup by a name
  or part of one (`discount`).
- `fuzzy_search` — when the exact spelling is unknown.
- `query_with_context` — code for a query in words or by name: what the Context
  Graph notes about the query point at, else the code ranked as
  `semantic_search` ranks it.
- `get_entity_details` and `get_relationships` — inspect an entity, and what it
  calls and is called by, uses, implements or inherits. Each relationship gives
  the file and line of both ends and the lines a call is made on, so it answers
  "who calls this, where?" with no language server; `file_path` tells entities
  of one name apart.
- `find_dead_code` and `analyze_complexity` — code-quality questions."#;

/// Build the Devscriptor workflow guidance block for the system prompt.
///
/// `server` is the configured MCP server name; tools are addressed as
/// `mcp__<server>__<tool>` once loaded via `select_tools`.
pub fn guidance(server: &str) -> String {
    format!(
        r#"# Devscriptor code intelligence

A Devscriptor MCP server named `{server}` is connected. It provides 75 tools for
code intelligence over a persistent SQLite knowledge graph. Its tools are
announced by name and description in `<tools_added>` blocks and must be loaded
with `select_tools` (exact name, e.g. `mcp__{server}__semantic_search`) before
you can call them. Load only the tools the current task needs.

Prefer these tools over blind file reading or grepping: they are indexed,
cheaper, and more precise. Never invent a Devscriptor tool name or parameter —
use only names from the announcements and the parameters in the loaded schema.

## Establish the index first
`mcp__{server}__get_statistics` reports whether a codebase has been indexed.
If it reports no entities, run `mcp__{server}__scan_codebase` (parameter
`path`) before searching; an index that has entities needs no new scan. For a
large repository prefer the background job tools — `start_analysis`, then poll
`get_analysis_progress` — instead of a long blocking call.
`mcp__{server}__health_check` reports whether embeddings and FTS5 are
available. The index follows your edits: Devscriptor watches the workspace and
re-checks the files behind every answer.

{FINDING_CODE}

## Precise navigation (LSP tools)
The 38 LSP tools (`get_hover`, `get_definition`, `get_references`,
`get_implementations`, `rename_symbol`, `get_diagnostics`, …) proxy a live
language server. The position tools take a symbol `name` — resolved from the
index, so the codebase must be scanned — or `file_path`, `line`, `character`
(all 0-indexed); prefer `name` to computing a column. LSP results carry `cite`
(`path:line:col`, 1-based); quote it as is. The call- and type-hierarchy tools
(`get_incoming_calls`, `get_outgoing_calls`, `get_subtypes`, `get_supertypes`)
take the symbol the same way. They need the language's server installed;
Devscriptor starts it on first use. Before the first LSP call in a project,
call `mcp__{server}__lsp_arch_detect` (parameter `project_path`); if a needed
server is missing, call `mcp__{server}__lsp_arch_setup` with
`auto_install: true`. The first LSP call after a server starts can be slow —
that is expected, do not retry immediately.

Use `get_definition` / `get_references` rather than grepping for a symbol, and
`rename_symbol` rather than hand-editing every call site. `get_references` adds,
in `index_locations`, the uses the index records that the server did not report
— in files it cannot read, such as `.vue` and `.svelte` files for the TypeScript
server — and `get_incoming_calls`, in `index_calls`, such calls with their
callers; they are matched by name, so check them. `rename_symbol`,
`format_document` and `get_range_format` write their edits to the files on
disk; re-read a file before editing it after one of them.

## Delegating
Your `explore` and `plan` sub-agents have Devscriptor's read-only tools, and
`coder` all of them, loaded the same way. When you brief one to map or search
code, tell it to start with them — `get_file_context` for a file,
`semantic_search` or `search_functions` for code by meaning or name,
`get_entity_details` and `get_relationships` for what an entity is and who calls
it — and to read files for the detail they point to.

## Context Graph — your long-term memory
Five tools persist notes across sessions in the same database — `add_context_note`,
`search_context`, `get_related_context`, `prune_context` and
`get_context_statistics`. (`describe_file`, `get_file_context` and
`query_with_context` read *about* code and are not note storage.) There are nine
note types: `adr`, `standard`, `optimization`, `failure`, `practice`,
`reflection`, `general`, `file_description`, `modification`.

- At the start of non-trivial work, call `mcp__{server}__search_context`
  (`search_type: "hybrid"`) to recall what is already known about the area. It
  also filters by `note_type`, `min_priority` and `status`.
  `get_related_context` retrieves notes linked to an entity type, and
  `describe_file` / `get_file_context` summarise what is known about one file.
- When you make an architectural decision, discover a non-obvious constraint,
  fix a subtle bug, or are corrected by the user, record it with
  `mcp__{server}__add_context_note`. Choose the matching `note_type`, set a
  `priority` (1–10), and link it to code via `related_entities` so it can be
  recalled later. Write the *why*, not what the diff already shows.
- Do not re-record something already in the Context Graph; search before adding.

## Failure handling
If a Devscriptor tool errors, diagnose before retrying. When a file's language
server does not run, the error names the server and says why, and what makes it
run: `lsp_arch_setup` installs one that is not installed, while one built for
another Ruby, or linked in from elsewhere, is fixed outside the session — say
so. "No LSP server available for file" means no language server reads that file
type (VB.NET, a MATLAB `.m`); answer from the index and the file.
"no project root above" means the file is not inside a project with the
manifest its server needs (`Cargo.toml`, a `.csproj`, …). An empty search result
means no match: rephrase, or ask `semantic_search` in words — unless
`get_statistics` shows an empty index, which needs a scan. A timeout on
`scan_codebase` means switch to `start_analysis` and poll. Do not repeat an
identical failing call."#
    )
}

/// The guidance for a sub-agent that has only Devscriptor's read-only tools
/// (`explore`, `plan`): the same search and navigation, and nothing about
/// scanning, editing or recording notes, which it cannot do.
pub fn read_only_guidance(server: &str) -> String {
    format!(
        r#"# Devscriptor code intelligence

A Devscriptor MCP server named `{server}` is connected. You have its read-only
tools: code search, LSP navigation and the Context Graph's notes, over a
persistent SQLite knowledge graph of the workspace. They are announced by name
and description in `<tools_added>` blocks and must be loaded with
`select_tools` (exact name, e.g. `mcp__{server}__semantic_search`) before you
can call them. Load only the tools the current task needs.

Prefer these tools over blind file reading or grepping: they are indexed,
cheaper, and more precise. To map an area, load them first and start there —
`get_file_context` for a file, `semantic_search` or `search_functions` for code
by meaning or name, `get_entity_details` and `get_relationships` for what an
entity is and who calls it — then read files for the detail they point to.
Never invent a Devscriptor tool name or parameter — use only names from the
announcements and the parameters in the loaded schema.

## The index
`mcp__{server}__get_statistics` reports whether the codebase has been indexed.
If it reports no entities, the index is empty: say so in your answer and work
with the file tools. `mcp__{server}__health_check` reports whether embeddings
and FTS5 are available. Devscriptor re-checks the files behind every answer, so
the index follows edits made while you work.

{FINDING_CODE}

## Precise navigation (LSP tools)
The LSP tools (`get_hover`, `get_definition`, `get_references`,
`get_implementations`, `get_diagnostics`, …) proxy a live language server. The
position tools take a symbol `name` — resolved from the index — or `file_path`,
`line`, `character` (all 0-indexed); prefer `name` to computing a column. LSP
results carry `cite` (`path:line:col`, 1-based); quote it as is. The call- and
type-hierarchy tools (`get_incoming_calls`, `get_outgoing_calls`,
`get_subtypes`, `get_supertypes`) take the symbol the same way. Before the
first LSP call in a project, call `mcp__{server}__lsp_arch_detect` (parameter
`project_path`): it reports the servers the project needs and starts the
installed ones. A missing server cannot be installed from here; say which one
is missing. The first LSP call after a server starts can be slow — that is
expected, do not retry immediately. Use `get_definition` / `get_references`
rather than grepping for a symbol; `get_references` adds, in
`index_locations`, uses the server did not report (in `.vue` and `.svelte`
files for the TypeScript server), and `get_incoming_calls`, in `index_calls`,
such calls with their callers, matched by name, so check them.

## Context Graph
Notes that earlier sessions recorded about the codebase live in the same
database. At the start of non-trivial work, call `mcp__{server}__search_context`
(`search_type: "hybrid"`) to recall what is already known about the area; it
also filters by `note_type`, `min_priority` and `status`.
`get_related_context` retrieves notes linked to an entity type, and
`describe_file` / `get_file_context` summarise what is known about one file.

## Failure handling
If a Devscriptor tool errors, diagnose before retrying. "No LSP server available
for file" means no server for that language is installed. "no project root
above" means the file is not inside a project with the manifest its server
needs (`Cargo.toml`, a `.csproj`, …). An empty search result means no match:
rephrase, or ask `semantic_search` in words. Do not repeat an identical failing
call."#
    )
}

/// The guidance in both forms, for the session's sub-agents.
pub fn subagent_guidance(server: &str) -> agent_core::McpGuidance {
    agent_core::McpGuidance {
        all: guidance(server),
        read_only: read_only_guidance(server),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `path`, written as a Unix absolute path, absolute on this platform:
    /// on Windows a path needs a drive to be absolute.
    fn absolute(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{path}"))
        } else {
            PathBuf::from(path)
        }
    }

    fn server(name: &str, command: Option<&str>, disabled: bool) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: command.map(String::from),
            disabled,
            ..Default::default()
        }
    }

    /// An installed Devscriptor, wherever the tests run.
    fn launch() -> DevscriptorLaunch {
        DevscriptorLaunch::cli(Path::new("/opt/ds/devscriptor"))
    }

    /// An `exists` predicate over a fixed set of installed files.
    ///
    /// The returned closure owns its data, so it does not borrow `files`.
    fn installed(files: &[&str]) -> impl Fn(&Path) -> bool + 'static {
        let owned: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
        move |p: &Path| owned.iter().any(|f| f == p)
    }

    fn dirs(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    // ---- search_dirs -----------------------------------------------------

    #[test]
    fn test_search_dirs_puts_path_entries_first_on_unix() {
        let home = PathBuf::from("/home/u");
        let found = search_dirs(Some("/a:/b"), Some(&home), false);
        assert_eq!(found[0], PathBuf::from("/a"));
        assert_eq!(found[1], PathBuf::from("/b"));
        assert!(found.contains(&PathBuf::from("/usr/local/bin")));
        assert!(found.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(found.contains(&PathBuf::from("/home/u/.local/bin")));
    }

    #[test]
    fn test_search_dirs_splits_on_semicolon_for_windows() {
        let found = search_dirs(Some(r"C:\a;C:\b"), None, true);
        assert_eq!(found[0], PathBuf::from(r"C:\a"));
        assert_eq!(found[1], PathBuf::from(r"C:\b"));
    }

    #[test]
    fn test_search_dirs_tolerates_missing_path_and_home() {
        let found = search_dirs(None, None, false);
        assert!(found.contains(&PathBuf::from("/usr/local/bin")));
    }

    // ---- find_launch: the four supported platforms -----------------------

    #[test]
    fn test_finds_universal_binary_on_linux_and_macos() {
        // Same layout for x86_64/aarch64 Linux and aarch64 macOS.
        let exists = installed(&["/usr/local/bin/devscriptor"]);
        let launch = find_launch(&dirs(&["/usr/local/bin"]), false, &exists).unwrap();
        assert_eq!(
            Path::new(&launch.command),
            Path::new("/usr/local/bin/devscriptor")
        );
        assert_eq!(launch.args, vec!["mcp".to_string()]);
    }

    // `PathBuf::join` uses the host separator, so the Windows cases below use
    // separator-neutral directories. What is under test is which *file name* is
    // chosen and how it is invoked, not Windows path syntax.
    #[test]
    fn test_finds_universal_binary_on_windows() {
        let exists = installed(&["/programs/devscriptor/devscriptor.exe"]);
        let launch = find_launch(&dirs(&["/programs/devscriptor"]), true, &exists).unwrap();
        assert_eq!(
            Path::new(&launch.command),
            Path::new("/programs/devscriptor/devscriptor.exe")
        );
        assert_eq!(launch.args, vec!["mcp".to_string()]);
    }

    #[test]
    fn test_windows_search_ignores_extensionless_unix_name() {
        // A bare `devscriptor` file must not be picked on Windows.
        let exists = installed(&["/programs/devscriptor/devscriptor"]);
        assert!(find_launch(&dirs(&["/programs/devscriptor"]), true, &exists).is_none());
    }

    #[test]
    fn test_never_looks_for_a_second_binary_name() {
        // Devscriptor ships one executable. A `devscriptor-mcp` entry point (the
        // old relative symlink / `.cmd` shim) must not be discovered — if only it
        // is present, Devscriptor is not installed.
        for names in [
            &["/usr/local/bin/devscriptor-mcp"][..],
            &["/usr/local/bin/devscriptor-mcp.cmd"][..],
            &["/usr/local/bin/devscriptor-mcp.exe"][..],
            &["/usr/local/bin/devscriptor-mcp-standalone"][..],
        ] {
            let exists = installed(names);
            for windows in [false, true] {
                assert!(
                    find_launch(&dirs(&["/usr/local/bin"]), windows, &exists).is_none(),
                    "must not discover {names:?} (windows={windows})"
                );
            }
        }
    }

    #[test]
    fn test_universal_binary_is_found_even_beside_a_stale_entry_point() {
        let exists = installed(&[
            "/usr/local/bin/devscriptor",
            "/usr/local/bin/devscriptor-mcp",
        ]);
        let launch = find_launch(&dirs(&["/usr/local/bin"]), false, &exists).unwrap();
        assert_eq!(
            Path::new(&launch.command),
            Path::new("/usr/local/bin/devscriptor")
        );
        assert_eq!(launch.args, vec!["mcp".to_string()]);
    }

    #[test]
    fn test_first_matching_directory_wins() {
        let exists = installed(&["/opt/x/devscriptor", "/usr/local/bin/devscriptor"]);
        let launch = find_launch(&dirs(&["/opt/x", "/usr/local/bin"]), false, &exists).unwrap();
        assert_eq!(
            Path::new(&launch.command),
            Path::new("/opt/x/devscriptor"),
            "PATH order is honoured"
        );
    }

    #[test]
    fn test_returns_none_when_not_installed() {
        let exists = installed(&["/usr/local/bin/something-else"]);
        assert!(find_launch(&dirs(&["/usr/local/bin"]), false, &exists).is_none());
    }

    #[test]
    fn test_unix_search_ignores_windows_names() {
        let exists = installed(&["/usr/local/bin/devscriptor.exe"]);
        assert!(find_launch(&dirs(&["/usr/local/bin"]), false, &exists).is_none());
    }

    // ---- explicit DEVSCRIPTOR_BIN ----------------------------------------

    #[test]
    fn test_explicit_path_always_appends_the_mcp_subcommand() {
        // Including for the standalone development binary, which ignores its args.
        for path in [
            "/opt/ds/devscriptor",
            r"C:\ds\devscriptor.exe",
            "/opt/ds/devscriptor-mcp-standalone",
        ] {
            let launch = DevscriptorLaunch::cli(Path::new(path));
            assert_eq!(launch.command, path);
            assert_eq!(launch.args, vec!["mcp".to_string()], "for {path}");
        }
    }

    // ---- injection --------------------------------------------------------

    #[test]
    fn test_injection_is_skipped_when_user_configured_devscriptor() {
        let mut servers = vec![server("devscriptor", Some("/custom/devscriptor"), false)];
        assert!(
            inject_devscriptor_server(&mut servers, Path::new("/tmp"), &McpDefaults::default())
                .is_none()
        );
        assert_eq!(servers.len(), 1, "user config must win");
    }

    #[test]
    fn test_injection_is_skipped_when_user_disabled_devscriptor() {
        let mut servers = vec![server("devscriptor", None, true)];
        assert!(
            inject_devscriptor_server(&mut servers, Path::new("/tmp"), &McpDefaults::default())
                .is_none()
        );
        assert_eq!(servers.len(), 1, "an explicit opt-out must be respected");
    }

    // ---- database resolution ----------------------------------------------

    /// **The index must be named explicitly, and absolutely.** A server spawned
    /// without `--database` can open a global index under
    /// `~/.local/share/devscriptor/` rather than the project's, so the agent
    /// would search a different database than the one `devscriptor scan`
    /// built — and that global one accumulates every project the binary has
    /// touched.
    #[test]
    fn the_project_index_is_named_explicitly_and_absolutely() {
        let db = resolve_database(&absolute("/srv/app"));
        assert_eq!(db, absolute("/srv/app/devscriptor.db"));
        assert!(
            db.is_absolute(),
            "a relative path would be resolved by the child"
        );
    }

    /// `--cwd` defaults to `.`, which must not leak into the path.
    #[test]
    fn a_dot_relative_root_resolves_without_a_dot_component() {
        let db = resolve_database(Path::new("."));
        assert!(db.is_absolute(), "{db:?}");
        assert!(
            !db.to_string_lossy().contains("/./"),
            "the log line must read cleanly: {db:?}"
        );
        assert!(db.ends_with("devscriptor.db"), "{db:?}");
    }

    /// An absolute root is taken as given.
    #[test]
    fn an_absolute_root_is_used_verbatim() {
        assert_eq!(
            resolve_database(&absolute("/home/u/proj")),
            absolute("/home/u/proj/devscriptor.db")
        );
    }

    /// The index is pinned by `--database`, but the server also resolves
    /// relative tool arguments and walks up to find a project root — and it
    /// spawns language servers that do the same. Left in Rusta's own
    /// directory (the shell's, not `--cwd`) it answers about the wrong tree.
    #[test]
    fn the_injected_server_runs_in_the_workspace() {
        let mut servers = Vec::new();
        inject(
            &mut servers,
            launch(),
            &absolute("/srv/app"),
            &McpDefaults::default(),
        );
        assert_eq!(servers[0].cwd, Some(absolute("/srv/app")));
    }

    /// Devscriptor's own timeouts are Rusta's opinion; the global `[mcp]`
    /// ones are the user's, and the only way to change them without
    /// configuring the whole server.
    #[test]
    fn the_global_timeouts_override_devscriptors_own() {
        let mut servers = Vec::new();
        inject(
            &mut servers,
            launch(),
            Path::new("/srv/app"),
            &McpDefaults::default(),
        );
        assert_eq!(servers[0].startup_timeout_ms, Some(STARTUP_TIMEOUT_MS));
        assert_eq!(servers[0].tool_timeout_ms, Some(TOOL_TIMEOUT_MS));

        let global = McpDefaults {
            startup_timeout_ms: Some(5_000),
            tool_timeout_ms: Some(600_000),
            ..McpDefaults::default()
        };
        let mut servers = Vec::new();
        inject(&mut servers, launch(), Path::new("/srv/app"), &global);
        assert_eq!(servers[0].startup_timeout_ms, Some(5_000));
        assert_eq!(servers[0].tool_timeout_ms, Some(600_000));
    }

    /// The workspace and the index must be derived from one root, or they can
    /// disagree.
    #[test]
    fn the_workspace_and_the_index_agree() {
        let mut servers = Vec::new();
        inject(
            &mut servers,
            launch(),
            Path::new("/srv/app"),
            &McpDefaults::default(),
        );
        let db = servers[0].args.last().expect("a --database argument");
        assert_eq!(
            Path::new(db).parent(),
            servers[0].cwd.as_deref(),
            "the index must sit in the directory the server runs in"
        );
    }

    #[test]
    fn a_relative_root_becomes_an_absolute_workspace() {
        let root = absolute_root(Path::new("."));
        assert!(root.is_absolute(), "{root:?}");
        assert!(!root.to_string_lossy().contains("/./"), "{root:?}");
    }

    /// `configured_server` gates the system-prompt guidance while
    /// `is_devscriptor_server` gates injection. If the first is stricter, a
    /// wrapper config yields a connected server the model was never taught to
    /// drive — no injection, and no guidance either.
    #[test]
    fn every_config_that_suppresses_injection_also_earns_guidance() {
        let wrapper = McpServerConfig {
            name: "ci".to_string(),
            command: Some("sh".to_string()),
            args: vec!["-c".to_string(), "devscriptor mcp".to_string()],
            ..Default::default()
        };
        let mut servers = vec![wrapper];
        assert!(
            inject_devscriptor_server(&mut servers, Path::new("/srv/app"), &McpDefaults::default())
                .is_none(),
            "the wrapper is recognised, so nothing is injected"
        );
        assert_eq!(
            configured_server(&servers).as_deref(),
            Some("ci"),
            "so it must be recognised for guidance too"
        );
    }

    #[test]
    fn a_relative_configured_cwd_resolves_against_the_workspace() {
        let mut servers = vec![McpServerConfig {
            name: "tools".to_string(),
            command: Some("srv".to_string()),
            cwd: Some(PathBuf::from("backend")),
            ..Default::default()
        }];
        normalize_server_cwds(&mut servers, &absolute("/srv/app"));
        assert_eq!(servers[0].cwd, Some(absolute("/srv/app/backend")));
    }

    #[test]
    fn an_unset_cwd_is_left_to_inherit() {
        let mut servers = vec![McpServerConfig {
            name: "tools".to_string(),
            command: Some("srv".to_string()),
            ..Default::default()
        }];
        normalize_server_cwds(&mut servers, Path::new("/srv/app"));
        assert_eq!(servers[0].cwd, None, "inheriting is the documented default");
    }

    /// A session opened in a monorepo package must not invent an empty index
    /// beside itself and ignore the repo-wide one `devscriptor scan` wrote.
    #[test]
    fn a_session_inside_a_package_finds_the_repository_index() {
        let present = installed(&["/repo/devscriptor.db", "/repo/.git"]);
        assert_eq!(
            find_index(Path::new("/repo/packages/api"), &present),
            PathBuf::from("/repo/devscriptor.db")
        );
    }

    #[test]
    fn a_local_index_beats_one_further_up() {
        let present = installed(&[
            "/repo/devscriptor.db",
            "/repo/packages/api/devscriptor.db",
            "/repo/.git",
        ]);
        assert_eq!(
            find_index(Path::new("/repo/packages/api"), &present),
            PathBuf::from("/repo/packages/api/devscriptor.db")
        );
    }

    /// Without a repository bound, one stray index in `$HOME` would capture
    /// every session run anywhere beneath it.
    #[test]
    fn the_walk_up_stops_at_the_repository_boundary() {
        let present = installed(&["/home/u/devscriptor.db", "/home/u/repo/.git"]);
        assert_eq!(
            find_index(Path::new("/home/u/repo/pkg"), &present),
            PathBuf::from("/home/u/repo/pkg/devscriptor.db"),
            "the index above the repo must not be adopted"
        );
    }

    #[test]
    fn an_unindexed_workspace_scans_into_itself() {
        let present = installed(&[]);
        assert_eq!(
            find_index(Path::new("/srv/app"), &present),
            PathBuf::from("/srv/app/devscriptor.db")
        );
    }

    #[test]
    fn test_is_devscriptor_server_matches_name_command_or_args() {
        assert!(is_devscriptor_server(&server("devscriptor", None, false)));
        assert!(is_devscriptor_server(&server(
            "code-intel",
            Some("/usr/bin/devscriptor-mcp"),
            false
        )));
        let via_args = McpServerConfig {
            name: "ci".to_string(),
            command: Some("cmd".to_string()),
            args: vec!["/C".to_string(), r"C:\x\devscriptor-mcp.cmd".to_string()],
            ..Default::default()
        };
        assert!(
            is_devscriptor_server(&via_args),
            "cmd shim must be recognised"
        );
        assert!(!is_devscriptor_server(&server(
            "zai_web_search",
            None,
            false
        )));
    }

    #[test]
    fn test_detects_by_server_name() {
        let servers = vec![server("devscriptor", None, false)];
        assert_eq!(configured_server(&servers).as_deref(), Some("devscriptor"));
    }

    #[test]
    fn test_detects_by_command_under_custom_name() {
        let servers = vec![server(
            "code-intel",
            Some("/usr/bin/devscriptor-mcp"),
            false,
        )];
        assert_eq!(configured_server(&servers).as_deref(), Some("code-intel"));
    }

    #[test]
    fn test_ignores_disabled_server() {
        let servers = vec![server("devscriptor", None, true)];
        assert!(configured_server(&servers).is_none());
    }

    #[test]
    fn test_ignores_unrelated_servers() {
        let servers = vec![server("zai_web_search", None, false)];
        assert!(configured_server(&servers).is_none());
    }

    #[test]
    fn test_guidance_uses_configured_server_name_in_tool_prefix() {
        let text = guidance("code-intel");
        assert!(text.contains("mcp__code-intel__semantic_search"));
        assert!(text.contains("mcp__code-intel__lsp_arch_setup"));
        assert!(text.contains("mcp__code-intel__add_context_note"));
        // Must not leak a hardcoded server name.
        assert!(!text.contains("mcp__devscriptor__"));
    }

    #[test]
    fn test_guidance_covers_the_three_tool_families_and_context_graph() {
        let text = guidance("devscriptor");
        assert!(text.contains("scan_codebase"));
        assert!(text.contains("lsp_arch_detect"));
        assert!(text.contains("search_context"));
        assert!(text.contains("start_analysis"));
        assert!(
            text.contains("`semantic_search` — the default"),
            "a question in words goes to the hybrid search"
        );
        assert!(
            text.contains("`query_with_context` — code for a query in words or by name"),
            "query_with_context answers a question as well as a name"
        );
        assert!(
            text.contains("75 tools"),
            "the advertised count must be right"
        );
    }

    /// Devscriptor keeps its index current by itself, so the model is not told
    /// to manage a watcher; and LSP positions are cited as Devscriptor spells
    /// them, not converted from 0-based by hand.
    #[test]
    fn test_guidance_says_the_index_follows_edits_and_to_quote_cite() {
        let text = guidance("devscriptor");
        assert!(text.contains("The index follows your edits"));
        assert!(
            !text.contains("start_watching"),
            "watching is not the model's job"
        );
        assert!(text.contains("LSP results carry `cite`"));
        assert!(text.contains("quote it as is"));
    }

    /// A sub-agent with only the read-only tools is shown how to search,
    /// navigate and recall notes, and is never told to use a tool it lacks.
    #[test]
    fn the_read_only_guidance_names_only_read_only_tools() {
        let text = read_only_guidance("code-intel");
        for tool in [
            "mcp__code-intel__semantic_search",
            "mcp__code-intel__get_statistics",
            "mcp__code-intel__lsp_arch_detect",
            "mcp__code-intel__search_context",
            "get_references",
            "query_with_context",
        ] {
            assert!(text.contains(tool), "{tool}");
        }
        for tool in [
            "scan_codebase",
            "start_analysis",
            "lsp_arch_setup",
            "rename_symbol",
            "format_document",
            "add_context_note",
            "prune_context",
        ] {
            assert!(!text.contains(tool), "{tool}");
        }
        assert!(!text.contains("mcp__devscriptor__"));

        let both = subagent_guidance("code-intel");
        assert_eq!(both.all, guidance("code-intel"));
        assert_eq!(both.read_only, text);
    }

    /// The agent briefing a sub-agent is told that `explore` and `plan` have
    /// Devscriptor's read-only tools, and a sub-agent how to map an area with
    /// them before reading files, rather than briefing them to read file after
    /// file. A relationship's lines answer "who calls this, where?" without a
    /// language server.
    #[test]
    fn sub_agents_are_briefed_to_start_from_the_index() {
        let main = guidance("code-intel");
        assert!(
            main.contains(
                "Your `explore` and `plan` sub-agents have Devscriptor's read-only tools"
            ),
            "{main}"
        );
        let read_only = read_only_guidance("code-intel");
        assert!(
            read_only.contains("To map an area, load them first and start there"),
            "{read_only}"
        );
        assert!(
            !read_only.contains("## Delegating"),
            "a sub-agent briefs none"
        );
        for text in [&main, &read_only] {
            assert!(text.contains("the lines a call is made on"), "{text}");
        }
    }

    /// The failure advice quotes errors Devscriptor actually returns, never
    /// one such as "LSP not initialized", which `devscriptor mcp` does not
    /// report.
    /// Both forms of the guidance say what the LSP tools add from the index:
    /// the uses in `index_locations` and the calls, with their callers, in
    /// `index_calls`, both to be checked.
    #[test]
    fn the_guidance_names_what_the_index_adds() {
        for text in [guidance("devscriptor"), read_only_guidance("devscriptor")] {
            assert!(text.contains("`index_locations`"), "{text}");
            assert!(
                text.contains("`get_incoming_calls`, in `index_calls`"),
                "{text}"
            );
        }
    }

    #[test]
    fn test_guidance_names_the_lsp_errors_devscriptor_returns() {
        let text = guidance("devscriptor");
        assert!(text.contains("No LSP server available"));
        assert!(text.contains("the error names the server and says why"));
        assert!(text.contains("no project root above"));
        assert!(!text.contains("LSP not initialized"));
    }
}
