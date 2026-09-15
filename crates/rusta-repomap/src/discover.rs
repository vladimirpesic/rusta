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

fn git_ls_files(root: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("ls-files")
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
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
