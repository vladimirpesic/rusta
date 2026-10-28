//! `glob`: find files by name pattern, a page at a time.
//!
//! Matches are files only, most recently modified first.
//! `.gitignore` and `.ignore` are honoured unless `include_ignored` is set;
//! hidden files are listed, version-control metadata never is. Results are
//! paged with `offset` / `limit` and held to the tool-output character budget,
//! and a page that stops early says which `offset` continues it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use lm_bridge::tool::Tool;
use serde_json::{Value, json};

use crate::agent_loop::types::{
    ExecutableTool, ExecutableToolContext, ExecutableToolResult, ToolAccesses,
};

/// Matches a call returns when it sets no limit.
pub const DEFAULT_GLOB_LIMIT: usize = 100;

/// How long one walk may run before the matches found so far are returned.
const WALK_TIMEOUT: Duration = Duration::from_secs(20);

/// Version-control metadata directories, skipped even with `include_ignored`.
const VCS_DIRECTORIES: &[&str] = &[".git", ".svn", ".hg", ".bzr", ".jj", ".sl"];

/// Room kept for the notes after the paths when a page is held to the
/// character budget.
const NOTE_RESERVE_CHARS: usize = 400;

pub struct GlobTool {
    tool: Tool,
}

impl GlobTool {
    pub fn new() -> Self {
        Self {
            tool: Tool::new(
                "glob",
                "Find files by glob pattern, most recently modified first. Files only — \
                 directories are never listed; to find one, glob for a file inside it. \
                 .gitignore and .ignore are respected unless include_ignored is set; hidden \
                 files are included, and VCS directories such as .git are always skipped. \
                 Returns up to limit matches (default 100) starting at offset; when more \
                 matches exist, the result says which offset continues it. Paths come back \
                 relative to the working directory, ready for read_file.",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern. Without a `/` it matches file names at any depth (`*.rs`); with one, paths below `path` (`src/*.ts` = directly in src, `src/**/*.ts` = at any depth). Brace alternatives work: `*.{ts,tsx}`."
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory to search, absolute or relative to the working directory. Defaults to the working directory."
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Number of matches to skip, to page through results. Defaults to 0."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum matches to return. Defaults to 100; 0 removes the count limit (the output size limit still applies)."
                        },
                        "head_limit": {
                            "type": "integer",
                            "description": "Alias of limit."
                        },
                        "include_ignored": {
                            "type": "boolean",
                            "description": "Also match files excluded by .gitignore or .ignore (build output, node_modules). VCS directories stay skipped. Defaults to false."
                        }
                    },
                    "required": ["pattern"]
                }),
            ),
        }
    }
}

impl Default for GlobTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExecutableTool for GlobTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    fn accesses(&self, input: &Value) -> ToolAccesses {
        let root = input
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.trim().is_empty())
            .unwrap_or(".");
        ToolAccesses::read_file(PathBuf::from(root))
    }

    fn approval_rule(&self, _input: &Value) -> &str {
        "silent"
    }

    async fn execute(
        &self,
        input: Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let request = match GlobRequest::from_input(&input) {
            Ok(request) => request,
            Err(e) => return ExecutableToolResult::error(e),
        };
        let root = match &request.path {
            Some(path) => match ctx.resolve_allowed_path(Path::new(path)) {
                Ok(root) => root,
                Err(e) => return ExecutableToolResult::error(e),
            },
            None => ctx.cwd.canonicalize().unwrap_or_else(|_| ctx.cwd.clone()),
        };
        if !root.exists() {
            return ExecutableToolResult::error(format!("{} does not exist", root.display()));
        }
        if !root.is_dir() {
            return ExecutableToolResult::error(format!(
                "{} is not a directory; `path` names the directory to search",
                root.display()
            ));
        }
        let matcher = match PatternMatcher::new(&request.pattern, &root) {
            Ok(matcher) => matcher,
            Err(e) => return ExecutableToolResult::error(e),
        };

        let include_ignored = request.include_ignored;
        let walk_root = root.clone();
        let walk = tokio::task::spawn_blocking(move || {
            walk_matches(&walk_root, &matcher, include_ignored, WALK_TIMEOUT)
        })
        .await;
        let walk = match walk {
            Ok(walk) => walk,
            Err(e) => return ExecutableToolResult::error(format!("glob failed: {e}")),
        };

        let cwd = ctx.cwd.canonicalize().unwrap_or_else(|_| ctx.cwd.clone());
        ExecutableToolResult::text(render(&walk, &request, &cwd, ctx.tool_output.max_chars))
    }
}

/// One call's arguments, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobRequest {
    pattern: String,
    path: Option<String>,
    offset: usize,
    /// `None` removes the count limit.
    limit: Option<usize>,
    include_ignored: bool,
}

impl GlobRequest {
    fn from_input(input: &Value) -> Result<Self, String> {
        let pattern = input
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or("pattern is required")?
            .to_string();
        let path = input
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        let offset = count_arg(input, "offset")?.unwrap_or(0);
        let limit = match count_arg(input, "head_limit")?.or(count_arg(input, "limit")?) {
            None => Some(DEFAULT_GLOB_LIMIT),
            Some(0) => None,
            Some(n) => Some(n),
        };
        let include_ignored = match input.get("include_ignored") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => true,
            Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => false,
            Some(other) => {
                return Err(format!(
                    "include_ignored must be true or false, got {other}"
                ));
            }
        };
        Ok(Self {
            pattern,
            path,
            offset,
            limit,
            include_ignored,
        })
    }
}

/// A non-negative integer argument; integral floats and numeric strings are
/// accepted too.
fn count_arg(input: &Value, key: &str) -> Result<Option<usize>, String> {
    let invalid = || format!("{key} must be a non-negative integer");
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => match (n.as_u64(), n.as_f64()) {
            (Some(u), _) => Ok(Some(usize::try_from(u).unwrap_or(usize::MAX))),
            (None, Some(f)) if f >= 0.0 && f.fract() == 0.0 && f < 9.0e15 => Ok(Some(f as usize)),
            _ => Err(invalid()),
        },
        Some(Value::String(s)) => s.trim().parse::<usize>().map(Some).map_err(|_| invalid()),
        Some(_) => Err(invalid()),
    }
}

/// A glob pattern compiled for one search root.
///
/// The semantics are `.gitignore`'s, as in ripgrep: `*` stops at `/`, `**`
/// crosses it, and a pattern without `/` matches the file name at any depth.
struct PatternMatcher {
    matcher: GlobMatcher,
    /// Match against the file name rather than the path below the root.
    name_only: bool,
}

impl PatternMatcher {
    fn new(pattern: &str, root: &Path) -> Result<Self, String> {
        let mut pattern = pattern.replace('\\', "/");
        if Path::new(&pattern).is_absolute() {
            // An absolute pattern is fine when it points into the root.
            let root = root.to_string_lossy().replace('\\', "/");
            let root = root.trim_end_matches('/');
            match pattern.strip_prefix(root) {
                Some(rest) if rest.starts_with('/') => pattern = rest[1..].to_string(),
                _ => {
                    return Err(format!(
                        "pattern {pattern} is absolute but not inside {root}; pass the directory \
                         as `path` and a relative pattern"
                    ));
                }
            }
        }
        while let Some(rest) = pattern.strip_prefix("./") {
            pattern = rest.to_string();
        }
        if pattern.is_empty() {
            return Err("pattern is empty".to_string());
        }
        let name_only = !pattern.contains('/');
        let glob = GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| format!("Invalid glob: {e}"))?;
        Ok(Self {
            matcher: glob.compile_matcher(),
            name_only,
        })
    }

    fn matches(&self, relative: &Path) -> bool {
        if self.name_only {
            relative
                .file_name()
                .is_some_and(|name| self.matcher.is_match(name))
        } else {
            self.matcher.is_match(relative)
        }
    }
}

/// What one walk found.
#[derive(Debug, Default)]
struct Walk {
    /// Matching files, most recently modified first, as absolute paths.
    matches: Vec<PathBuf>,
    /// The walk hit its deadline; `matches` is partial.
    timed_out: bool,
    /// Entries that could not be read, and the first such error.
    unreadable: usize,
    first_error: Option<String>,
}

fn walk_matches(
    root: &Path,
    matcher: &PatternMatcher,
    include_ignored: bool,
    timeout: Duration,
) -> Walk {
    let deadline = Instant::now() + timeout;
    let mut walk = Walk::default();
    let mut found: Vec<(Option<SystemTime>, PathBuf)> = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(!include_ignored)
        // Dotfiles are part of a project (`.github`, `.cargo/config.toml`);
        // only version-control metadata is kept out, below.
        .hidden(false)
        .filter_entry(|entry| {
            let is_vcs_dir = entry.depth() > 0
                && entry.file_type().is_some_and(|t| t.is_dir())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| VCS_DIRECTORIES.contains(&name));
            !is_vcs_dir
        })
        .build();

    for (seen, entry) in walker.enumerate() {
        if seen % 256 == 0 && Instant::now() >= deadline {
            walk.timed_out = true;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                walk.unreadable += 1;
                walk.first_error.get_or_insert_with(|| e.to_string());
                continue;
            }
        };
        let is_file = entry
            .file_type()
            .is_some_and(|t| t.is_file() || (t.is_symlink() && entry.path().is_file()));
        if !is_file {
            continue;
        }
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if !matcher.matches(relative) {
            continue;
        }
        let modified = entry.metadata().ok().and_then(|m| m.modified().ok());
        found.push((modified, entry.into_path()));
    }

    // Newest first; files without a timestamp last; ties by path, so pages are
    // stable while nothing changes.
    found.sort_by(|(a_time, a_path), (b_time, b_path)| {
        b_time.cmp(a_time).then_with(|| a_path.cmp(b_path))
    });
    walk.matches = found.into_iter().map(|(_, path)| path).collect();
    walk
}

/// `path` as shown to the model: relative to the working directory when it is
/// inside it, its parts joined with `/` as in the patterns on every platform,
/// so it can be passed straight to `read_file`; absolute otherwise.
fn display_path(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).map_or_else(
        |_| path.display().to_string(),
        |rel| {
            rel.components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        },
    )
}

fn render(walk: &Walk, request: &GlobRequest, cwd: &Path, max_chars: usize) -> String {
    let total = walk.matches.len();
    let mut notes = Vec::new();
    if walk.timed_out {
        notes.push(format!(
            "[The search timed out after {} s; these results are partial. Narrow the pattern or \
             the path.]",
            WALK_TIMEOUT.as_secs()
        ));
    }
    if walk.unreadable > 0 {
        notes.push(format!(
            "[{} entries could not be read and were skipped (first error: {}).]",
            walk.unreadable,
            walk.first_error.as_deref().unwrap_or("unknown")
        ));
    }

    let mut lines = Vec::new();
    if total == 0 {
        let hint = if request.include_ignored {
            ""
        } else {
            " Files excluded by .gitignore or .ignore were not searched; set include_ignored \
             to search them too."
        };
        lines.push(format!("No files matched.{hint}"));
    } else if request.offset >= total {
        lines.push(format!(
            "[No matches at offset={}: there are {total}.]",
            request.offset
        ));
    } else {
        let end = request.limit.map_or(total, |limit| {
            request.offset.saturating_add(limit).min(total)
        });
        let budget = if max_chars == 0 {
            usize::MAX
        } else {
            max_chars.saturating_sub(NOTE_RESERVE_CHARS).max(1)
        };
        let mut used = 0usize;
        for path in &walk.matches[request.offset..end] {
            let shown = display_path(path, cwd);
            let cost = shown.chars().count() + 1;
            if used + cost > budget && !lines.is_empty() {
                break;
            }
            used += cost;
            lines.push(shown);
        }
        let shown_end = request.offset + lines.len();
        let size_limited = shown_end < end;
        if shown_end < total {
            let why = if size_limited {
                " (output size limit reached)"
            } else {
                ""
            };
            notes.push(format!(
                "[Showing matches {}–{shown_end} of {total}{why}; more matches exist — call again \
                 with offset={shown_end}.]",
                request.offset + 1
            ));
        } else if request.offset > 0 {
            notes.push(format!(
                "[Showing matches {}–{shown_end} of {total}.]",
                request.offset + 1
            ));
        }
    }

    let mut text = lines.join("\n");
    if !notes.is_empty() {
        text.push_str("\n\n");
        text.push_str(&notes.join("\n"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn ctx(dir: &Path) -> ExecutableToolContext {
        ExecutableToolContext {
            cwd: dir.to_path_buf(),
            ..Default::default()
        }
    }

    async fn glob(ctx: &ExecutableToolContext, args: Value) -> ExecutableToolResult {
        GlobTool::new().execute(args, ctx, "g").await
    }

    fn touch(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    /// Sets `rel`'s modification time to `secs` after the epoch.
    fn age(root: &Path, rel: &str, secs: u64) {
        std::fs::File::options()
            .write(true)
            .open(root.join(rel))
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn listed(result: &ExecutableToolResult) -> Vec<String> {
        let text = result.text_content();
        text.split("\n\n")
            .next()
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    async fn matches_come_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        for (name, secs) in [("old.rs", 1_000), ("new.rs", 3_000), ("mid.rs", 2_000)] {
            touch(dir.path(), name);
            age(dir.path(), name, secs);
        }
        let result = glob(&ctx(dir.path()), json!({"pattern": "*.rs"})).await;
        assert_eq!(listed(&result), ["new.rs", "mid.rs", "old.rs"]);
    }

    #[tokio::test]
    async fn pages_continue_from_the_offset_they_name() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            let name = format!("f{i}.txt");
            touch(dir.path(), &name);
            age(dir.path(), &name, 1_000 + i);
        }
        let ctx = ctx(dir.path());

        let first = glob(&ctx, json!({"pattern": "*.txt", "limit": 2})).await;
        assert_eq!(
            first.text_content(),
            "f4.txt\nf3.txt\n\n[Showing matches 1–2 of 5; more matches exist — call again with \
             offset=2.]"
        );
        let second = glob(
            &ctx,
            json!({"pattern": "*.txt", "head_limit": 2, "offset": 2}),
        )
        .await;
        assert_eq!(listed(&second), ["f2.txt", "f1.txt"]);
        assert!(second.text_content().contains("offset=4"));
        let last = glob(&ctx, json!({"pattern": "*.txt", "limit": 2, "offset": 4})).await;
        assert_eq!(last.text_content(), "f0.txt\n\n[Showing matches 5–5 of 5.]");
        let past = glob(&ctx, json!({"pattern": "*.txt", "offset": 9})).await;
        assert_eq!(
            past.text_content(),
            "[No matches at offset=9: there are 5.]"
        );
        let all = glob(&ctx, json!({"pattern": "*.txt", "limit": 0})).await;
        assert_eq!(listed(&all).len(), 5);
    }

    #[tokio::test]
    async fn patterns_follow_gitignore_semantics() {
        let dir = tempfile::tempdir().unwrap();
        for rel in ["top.rs", "src/lib.rs", "src/deep/mod.rs", "docs/a.md"] {
            touch(dir.path(), rel);
        }
        let ctx = ctx(dir.path());
        let set = |r: &ExecutableToolResult| listed(r).into_iter().collect::<HashSet<_>>();

        // No `/`: a file name at any depth.
        let any_depth = glob(&ctx, json!({"pattern": "*.rs"})).await;
        assert_eq!(
            set(&any_depth),
            HashSet::from([
                "top.rs".into(),
                "src/lib.rs".into(),
                "src/deep/mod.rs".into()
            ])
        );
        // `*` stops at `/`; `**` crosses it.
        let one_level = glob(&ctx, json!({"pattern": "src/*.rs"})).await;
        assert_eq!(set(&one_level), HashSet::from(["src/lib.rs".into()]));
        let recursive = glob(&ctx, json!({"pattern": "./src/**/*.rs"})).await;
        assert_eq!(
            set(&recursive),
            HashSet::from(["src/lib.rs".into(), "src/deep/mod.rs".into()])
        );
        let braces = glob(&ctx, json!({"pattern": "*.{md,rs}", "limit": 0})).await;
        assert_eq!(listed(&braces).len(), 4);
    }

    #[tokio::test]
    async fn directories_are_never_listed() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "src/main.rs");
        let result = glob(&ctx(dir.path()), json!({"pattern": "src"})).await;
        assert!(result.text_content().starts_with("No files matched."));
    }

    #[tokio::test]
    async fn path_scopes_the_search_and_paths_stay_cwd_relative() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a/one.rs");
        touch(dir.path(), "b/two.rs");
        let result = glob(&ctx(dir.path()), json!({"pattern": "*.rs", "path": "b"})).await;
        assert_eq!(listed(&result), ["b/two.rs"]);
    }

    #[tokio::test]
    async fn a_path_outside_the_allowed_roots_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let result = glob(
            &ctx(dir.path()),
            json!({"pattern": "*", "path": outside.path()}),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result
                .text_content()
                .contains("outside allowed directories")
        );
    }

    #[tokio::test]
    async fn ignored_files_need_include_ignored_and_vcs_metadata_never_shows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".ignore"), "build/\n").unwrap();
        touch(dir.path(), "src/app.js");
        touch(dir.path(), "build/app.js");
        touch(dir.path(), ".git/hooks/pre-commit.js");
        touch(dir.path(), ".github/workflows/ci.js");
        let ctx = ctx(dir.path());
        let set = |r: &ExecutableToolResult| listed(r).into_iter().collect::<HashSet<_>>();

        let default = glob(&ctx, json!({"pattern": "**/*.js"})).await;
        assert_eq!(
            set(&default),
            HashSet::from(["src/app.js".into(), ".github/workflows/ci.js".into()])
        );
        let everything = glob(&ctx, json!({"pattern": "**/*.js", "include_ignored": true})).await;
        assert_eq!(
            set(&everything),
            HashSet::from([
                "src/app.js".into(),
                "build/app.js".into(),
                ".github/workflows/ci.js".into()
            ])
        );
        let none = glob(&ctx, json!({"pattern": "*.nothing"})).await;
        assert!(none.text_content().contains("set include_ignored"));
    }

    #[tokio::test]
    async fn pages_stay_within_the_output_budget() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..200 {
            touch(
                dir.path(),
                &format!("some/longer/directory/name/file-{i:03}.txt"),
            );
        }
        let mut ctx = ctx(dir.path());
        ctx.tool_output.max_chars = 2_000;
        let result = glob(&ctx, json!({"pattern": "*.txt", "limit": 0})).await;
        let text = result.text_content();
        assert!(text.chars().count() <= 2_000, "{}", text.chars().count());
        assert!(
            text.contains("(output size limit reached); more matches exist"),
            "{text}"
        );
        let shown = listed(&result).len();
        assert!(text.contains(&format!("offset={shown}")), "{text}");
    }

    #[tokio::test]
    async fn bad_arguments_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        assert!(glob(&ctx, json!({})).await.is_error);
        let bad_glob = glob(&ctx, json!({"pattern": "a[b"})).await;
        assert!(bad_glob.text_content().starts_with("Invalid glob"));
        let bad_offset = glob(&ctx, json!({"pattern": "*", "offset": -1})).await;
        assert!(bad_offset.text_content().contains("offset must be"));
        touch(dir.path(), "file.txt");
        let not_dir = glob(&ctx, json!({"pattern": "*", "path": "file.txt"})).await;
        assert!(not_dir.text_content().contains("is not a directory"));
    }

    #[tokio::test]
    async fn an_absolute_pattern_inside_the_root_is_made_relative() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "src/main.rs");
        let root = dir.path().canonicalize().unwrap();
        let pattern = format!("{}/src/*.rs", root.display());
        let result = glob(&ctx(dir.path()), json!({"pattern": pattern})).await;
        assert_eq!(listed(&result), ["src/main.rs"]);
    }

    #[test]
    fn a_walk_past_its_deadline_is_partial() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt");
        let matcher = PatternMatcher::new("*", dir.path()).unwrap();
        let walk = walk_matches(dir.path(), &matcher, false, Duration::ZERO);
        assert!(walk.timed_out);
    }
}
