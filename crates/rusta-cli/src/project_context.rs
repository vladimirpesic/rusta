//! Project context for the system prompt: instruction files (`AGENTS.md`,
//! `CLAUDE.md`, …) within a token budget, and listings of the working
//! directory and any additional directories.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::expand_tilde;

/// Default file names to search for when looking for project context.
const DEFAULT_PROJECT_CONTEXT_FILES: &[&str] =
    &["AGENTS.md", "CLAUDE.md", "agents.md", "claude.md"];
/// Default token budget for instruction files (chars / 4 heuristic). Large
/// enough for a thorough `AGENTS.md`; a file cut short is named in a warning.
pub const DEFAULT_PROJECT_CONTEXT_BUDGET_TOKENS: u64 = 8192;
/// Maximum entries in an auto-generated directory listing.
const MAX_DIR_LISTING_ENTRIES: usize = 100;

/// A single piece of project context with provenance.
#[derive(Debug, Clone)]
struct ContextPiece {
    source: PathBuf,
    text: String,
}

/// What the project contributes to the system prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectContext {
    /// The instruction files, each annotated with its path, within the token
    /// budget.
    pub instructions: Option<String>,
    /// Instruction files cut short, or left out, to fit the budget.
    pub truncated: Vec<PathBuf>,
    /// Each instruction file read, once, with its text: what `/reload`
    /// compares to tell the model which ones changed.
    pub files: Vec<(PathBuf, String)>,
    /// A top-level listing of the working directory.
    pub cwd_listing: Option<String>,
    /// Each additional directory (`--add-dir`) and its top-level listing.
    pub additional_dirs: Vec<(PathBuf, String)>,
}

/// Load the project context for the system prompt.
///
/// Instruction files, lowest precedence first (a later file wins where they
/// disagree):
/// 1. User-level instruction files (`~/.config/rusta/AGENTS.md`,
///    `~/.agents/AGENTS.md`).
/// 2. Project files discovered by walking upward from `cwd`, ordered
///    root-to-leaf so the deepest directory overrides parents.
/// 3. Any `AGENTS.md` inside an additional directory (`--add-dir`).
/// 4. Explicit `--project-context-file` paths.
///
/// Each file is annotated with `<!-- From: {path} -->`, and together they
/// are cut to the token budget; a file that does not fit is named in a
/// warning and in [`ProjectContext::truncated`].
pub fn load_project_context(
    cwd: &Path,
    project_context_files: &[PathBuf],
    additional_dirs: &[PathBuf],
    budget_tokens: Option<u64>,
) -> ProjectContext {
    let budget_tokens = budget_tokens.unwrap_or(DEFAULT_PROJECT_CONTEXT_BUDGET_TOKENS);
    let max_chars = budget_tokens.saturating_mul(4) as usize;

    let mut pieces = Vec::new();
    pieces.extend(user_context_pieces());
    pieces.extend(project_context_pieces(cwd));
    let mut listings = Vec::new();
    for dir in additional_dirs {
        let dir = expand_tilde(dir);
        pieces.extend(additional_dir_pieces(&dir));
        if let Some(listing) = dir_listing(&dir) {
            listings.push((dir, listing));
        }
    }
    for path in project_context_files {
        let path = expand_tilde(path);
        if let Some(text) = read_file_trimmed(&path) {
            pieces.push(ContextPiece { source: path, text });
        }
    }

    let pieces = distinct(pieces);
    let (instructions, truncated) = assemble_instructions(&pieces, max_chars);
    for path in &truncated {
        tracing::warn!(
            "{} does not fit the project-context budget of {budget_tokens} tokens and was cut; \
             raise project_context_budget_tokens to include all of it",
            path.display()
        );
    }
    ProjectContext {
        instructions,
        truncated,
        files: pieces
            .into_iter()
            .map(|piece| (piece.source, piece.text))
            .collect(),
        cwd_listing: dir_listing(cwd),
        additional_dirs: listings,
    }
}

/// `pieces` with each file once, where it first comes.
fn distinct(pieces: Vec<ContextPiece>) -> Vec<ContextPiece> {
    let mut seen = HashSet::new();
    pieces
        .into_iter()
        .filter(|piece| seen.insert(canonical_source(&piece.source)))
        .collect()
}

/// Join the instruction files within `max_chars`. Returns the text and the
/// files cut short or left out.
fn assemble_instructions(
    pieces: &[ContextPiece],
    max_chars: usize,
) -> (Option<String>, Vec<PathBuf>) {
    let mut output = Vec::new();
    let mut truncated = Vec::new();
    let mut total_chars = 0usize;

    for piece in pieces {
        let remaining = max_chars.saturating_sub(total_chars);
        if remaining == 0 {
            truncated.push(piece.source.clone());
            continue;
        }
        let annotated = annotate(&piece.text, &piece.source);
        let (chunk, was_truncated) = truncate_text(&annotated, remaining);
        if was_truncated {
            truncated.push(piece.source.clone());
        }
        total_chars += chunk.chars().count();
        output.push(chunk);
    }

    if output.is_empty() {
        return (None, truncated);
    }
    if !truncated.is_empty() {
        output.push(
            "<!-- Note: project instructions were cut short here to fit the context budget. -->"
                .to_string(),
        );
    }
    (Some(output.join("\n\n")), truncated)
}

/// Wrap a piece of text with a `<!-- From: ... -->` annotation.
fn annotate(text: &str, source: &Path) -> String {
    format!("<!-- From: {} -->\n{}", source.display(), text)
}

/// Truncate text to a character limit, returning whether truncation occurred.
fn truncate_text(text: &str, limit: usize) -> (String, bool) {
    let char_count = text.chars().count();
    if char_count <= limit {
        (text.to_string(), false)
    } else {
        (text.chars().take(limit).collect(), true)
    }
}

/// Return the canonical source path for deduplication, falling back to the
/// original path if canonicalization fails.
fn canonical_source(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Read a file and return trimmed contents, or `None` if it cannot be read.
fn read_file_trimmed(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Collect user-level instruction files.
fn user_context_pieces() -> Vec<ContextPiece> {
    let mut pieces = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for name in DEFAULT_PROJECT_CONTEXT_FILES {
            let path = home.join(".agents").join(name);
            if let Some(text) = read_file_trimmed(&path) {
                pieces.push(ContextPiece { source: path, text });
            }
        }
    }
    if let Some(config_dir) = agent_core::user_dirs::config_dir() {
        for name in DEFAULT_PROJECT_CONTEXT_FILES {
            let path = config_dir.join("rusta").join(name);
            if let Some(text) = read_file_trimmed(&path) {
                pieces.push(ContextPiece { source: path, text });
            }
        }
    }
    pieces
}

/// Collect project instruction files by walking upward from `cwd`.
///
/// Returns pieces in root-to-leaf order so deeper directories override
/// ancestors when assembled.
fn project_context_pieces(cwd: &Path) -> Vec<ContextPiece> {
    let mut candidates = Vec::new();
    let mut dir = cwd.to_path_buf();
    loop {
        let mut level = Vec::new();
        for name in DEFAULT_PROJECT_CONTEXT_FILES {
            level.push(dir.join(name));
        }
        level.push(dir.join(".rusta").join("AGENTS.md"));
        level.push(dir.join(".rusta").join("CLAUDE.md"));
        level.push(dir.join(".rusta").join("agents.md"));
        level.push(dir.join(".rusta").join("claude.md"));
        candidates.push(level);
        if !dir.pop() {
            break;
        }
    }
    candidates.reverse();

    let mut pieces = Vec::new();
    for level in candidates {
        for path in level {
            if let Some(text) = read_file_trimmed(&path) {
                pieces.push(ContextPiece { source: path, text });
            }
        }
    }
    pieces
}

/// The instruction files inside an additional directory.
fn additional_dir_pieces(dir: &Path) -> Vec<ContextPiece> {
    let mut pieces = Vec::new();
    if !dir.is_dir() {
        return pieces;
    }
    for name in DEFAULT_PROJECT_CONTEXT_FILES {
        let path = dir.join(name);
        if let Some(text) = read_file_trimmed(&path) {
            pieces.push(ContextPiece { source: path, text });
        }
    }
    pieces
}

/// Build a concise top-level directory listing.
fn dir_listing(dir: &Path) -> Option<String> {
    let mut entries: Vec<(String, u64)> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                return None;
            }
            let size = if e.file_type().ok()?.is_file() {
                e.metadata().ok()?.len()
            } else {
                0
            };
            Some((name, size))
        })
        .collect();

    if entries.is_empty() {
        return None;
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut lines: Vec<String> = entries
        .iter()
        .take(MAX_DIR_LISTING_ENTRIES)
        .map(|(name, size)| {
            if *size > 0 {
                format!("{} ({})", name, human_size(*size))
            } else {
                name.clone()
            }
        })
        .collect();

    if entries.len() > MAX_DIR_LISTING_ENTRIES {
        lines.push(format!(
            "... and {} more entries",
            entries.len() - MAX_DIR_LISTING_ENTRIES
        ));
    }

    Some(lines.join("\n"))
}

/// Convert a byte size to a short human-readable string.
fn human_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB"];
    let mut value = size as f64;
    let mut unit = UNITS[0];
    for &next in &UNITS[1..] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1}{unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(cwd: &Path) -> ProjectContext {
        load_project_context(cwd, &[], &[], None)
    }

    fn instructions(ctx: &ProjectContext) -> &str {
        ctx.instructions
            .as_deref()
            .expect("instructions were found")
    }

    #[test]
    fn test_loads_agents_md_from_cwd() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "Always use Rust.").unwrap();

        let ctx = load(temp.path());
        assert!(instructions(&ctx).contains("Always use Rust."));
        assert!(instructions(&ctx).contains("<!-- From:"));
        assert!(ctx.truncated.is_empty());
    }

    #[test]
    fn test_child_overrides_parent() {
        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "Parent rules.").unwrap();
        std::fs::write(child.join("AGENTS.md"), "Child rules.").unwrap();

        let ctx = load(&child);
        let text = instructions(&ctx);
        assert!(text.find("Child rules.").unwrap() > text.find("Parent rules.").unwrap());
    }

    #[test]
    fn test_explicit_files_take_precedence() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "Default.").unwrap();
        let explicit = temp.path().join("CUSTOM.md");
        std::fs::write(&explicit, "Explicit.").unwrap();

        let ctx = load_project_context(temp.path(), &[explicit], &[], None);
        let text = instructions(&ctx);
        assert!(text.find("Explicit.").unwrap() > text.find("Default.").unwrap());
    }

    #[test]
    fn test_deduplication() {
        let temp = tempfile::tempdir().unwrap();
        let explicit = temp.path().join("AGENTS.md");
        std::fs::write(&explicit, "Only once.").unwrap();

        let ctx = load_project_context(
            temp.path(),
            &[explicit.clone(), explicit.clone()],
            &[],
            None,
        );
        assert_eq!(instructions(&ctx).matches("Only once.").count(), 1);
        let read: Vec<_> = ctx
            .files
            .iter()
            .filter(|(path, _)| path == &explicit)
            .collect();
        assert_eq!(read, [&(explicit, "Only once.".to_string())]);
    }

    #[test]
    fn test_cwd_listing_is_its_own_part() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("README.md"), "# Hi").unwrap();

        let ctx = load(temp.path());
        assert!(ctx.cwd_listing.as_deref().unwrap().contains("README.md"));
        assert!(
            ctx.instructions.is_none(),
            "a listing is not an instruction"
        );
    }

    #[test]
    fn test_additional_dir() {
        let temp = tempfile::tempdir().unwrap();
        let extra = temp.path().join("extra");
        std::fs::create_dir(&extra).unwrap();
        std::fs::write(extra.join("NOTE.md"), "note").unwrap();
        std::fs::write(extra.join("AGENTS.md"), "Extra rules.").unwrap();

        let ctx = load_project_context(temp.path(), &[], std::slice::from_ref(&extra), None);
        let (dir, listing) = &ctx.additional_dirs[0];
        assert_eq!(dir, &extra);
        assert!(listing.contains("NOTE.md"));
        assert!(instructions(&ctx).contains("Extra rules."));
    }

    /// A file cut to the budget is named, so the warning can say which.
    #[test]
    fn test_budget_truncation_names_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let agents = temp.path().join("AGENTS.md");
        std::fs::write(&agents, "Rust. ".repeat(50)).unwrap();

        let ctx = load_project_context(temp.path(), &[], &[], Some(10));
        assert_eq!(ctx.truncated, [agents]);
        let text = instructions(&ctx);
        assert!(text.contains("cut short here"), "{text}");
        assert!(text.chars().count() < 150, "{text}");
    }

    #[test]
    fn the_default_budget_holds_a_long_agents_md() {
        let temp = tempfile::tempdir().unwrap();
        // About 7,000 tokens: under the 8,192-token default.
        std::fs::write(temp.path().join("AGENTS.md"), "word ".repeat(5_600)).unwrap();
        let ctx = load(temp.path());
        assert!(ctx.truncated.is_empty());
    }

    #[test]
    fn test_nothing_found_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = load(temp.path());
        assert!(ctx.instructions.is_none());
        assert!(ctx.cwd_listing.is_none());
        assert!(ctx.additional_dirs.is_empty());
    }

    #[test]
    fn test_truncate_text_respects_character_boundaries() {
        let text = "aé😀b"; // 4 chars, 7 bytes
        assert_eq!(truncate_text(text, 4), (text.to_string(), false));
        assert_eq!(truncate_text(text, 3), ("aé😀".to_string(), true));
        assert_eq!(truncate_text(text, 2), ("aé".to_string(), true));
        assert_eq!(truncate_text(text, 0), ("".to_string(), true));
    }

    #[test]
    fn test_budget_truncation_with_multibyte_characters() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("AGENTS.md"), "Rúst 🔧.").unwrap();
        // Must not panic when cutting multibyte content.
        let ctx = load_project_context(temp.path(), &[], &[], Some(1));
        assert_eq!(ctx.truncated.len(), 1);
    }
}
