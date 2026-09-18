//! Source-file discovery — ADR.md §6.5 step 1.
//!
//! Git-tracked files when a git repo (`git ls-files` — `.gitignore` handled
//! by git itself); otherwise a recursive walk skipping the built-in ignores.

use crate::lang::{IGNORED_DIRS, Lang};
use std::path::Path;

/// Discover map-eligible sources under `root` as sorted repo-relative paths
/// (forward slashes, no `./` prefix).
pub(crate) fn source_files(root: &Path) -> Vec<String> {
    let files = git_ls_files(root).unwrap_or_else(|| {
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out
    });
    let mut rels: Vec<String> = files
        .into_iter()
        .filter(|f| Lang::from_path(Path::new(f)).is_some())
        .map(|f| f.trim_start_matches("./").replace('\\', "/"))
        .collect();
    rels.sort_unstable();
    rels.dedup();
    rels
}

/// True when `root/rel` resolves *inside* `root` — the §6.12 read-side
/// fence, applied to the map.
///
/// This mirrors `rusta_edit::contains_path` rather than calling it:
/// `rusta-repomap` is a leaf crate with no `rusta-*` dependencies, and
/// making the map depend on the *edit* crate for a filesystem predicate
/// would invert the dependency graph for eight lines of `std`. The
/// duplication is deliberate and should be resolved by moving the
/// predicate somewhere both can reach, not by adding that edge.
///
/// A non-existent path canonicalizes to `Err` and is treated as outside.
/// That matches the map's existing behaviour for tracked-but-deleted files,
/// which it already skips.
pub(crate) fn within_root(root: &Path, rel: &str) -> bool {
    let Ok(real_root) = root.canonicalize() else {
        return true; // unknowable root — as in `contains_path`
    };
    root.join(rel)
        .canonicalize()
        .is_ok_and(|real| real.starts_with(&real_root))
}

fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    // A33: plain `ls-files` C-quotes any non-ASCII path (`core.quotepath`
    // defaults on), so `src/café.rs` arrived as the literal
    // `"src/caf\303\251.rs"`, matched no file on disk, and vanished from the
    // map with no warning — while the non-git walk fallback handled the same
    // name fine, so git and non-git repos disagreed about what the repo
    // contains. `-z` turns off quoting entirely and NUL-separates, which
    // also makes paths containing newlines unambiguous.
    let out = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        out.stdout
            .split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect(),
    )
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let name = entry.file_name();
            if !IGNORED_DIRS.contains(&name.to_string_lossy().as_ref()) {
                walk(root, &path, out);
            }
        } else if path.strip_prefix(root).is_ok() && Lang::from_path(&path).is_some() {
            out.push(
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}
