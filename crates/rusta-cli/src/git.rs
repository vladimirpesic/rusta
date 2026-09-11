//! Git integration — development plan §6.9.
//!
//! Auto-commit after each applied edit batch with message
//! `rusta: <one-line summary of the user request>`; `/undo` reverts the batch
//! commit via the journal. The safety rule is structural: a commit is only
//! ever reset when `HEAD` **is** the rusta commit being undone — unrelated
//! commits are never touched (§6.9: "never `git reset` on unrelated
//! commits"). Everything is plain `git` subprocess calls (the §10 dependency
//! set has no git2); a missing git binary or a non-repo degrades to
//! journal-only undo, never an error.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Prefix of every auto-commit message (§6.9).
pub const COMMIT_PREFIX: &str = "rusta: ";

/// Cap on `/diff` output lines (the user-facing surface, not model context).
const DIFF_MAX_LINES: usize = 200;

/// The git side of a working tree — best-effort by design.
#[derive(Debug)]
pub struct Git {
    root: PathBuf,
    available: bool,
}

impl Git {
    /// Probes `root` for a git work tree. When git is missing or `root` is
    /// outside any repo, every operation becomes a graceful no-op.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let available = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&root)
            .output()
            .is_ok_and(|out| out.status.success());
        Self { root, available }
    }

    /// Whether git operations are live.
    pub fn available(&self) -> bool {
        self.available
    }

    /// The work-tree root (also resolving subdirectory launches to the repo
    /// root, which is what §6.12's "cwd = repo root" wants).
    pub fn toplevel(&self) -> PathBuf {
        if !self.available {
            return self.root.clone();
        }
        run(&self.root, ["rev-parse", "--show-toplevel"])
            .ok()
            .map(|out| PathBuf::from(trim_newline(&out)))
            .unwrap_or_else(|| self.root.clone())
    }

    /// Commits exactly `paths` with `message` (`git commit --only --`):
    /// the batch paths are taken from the working tree and nothing else is
    /// committed — unrelated work the user staged beforehand stays staged,
    /// never absorbed into a `rusta:` commit. Returns the commit SHA, or
    /// `None` when git is unavailable or the commit failed — the edit itself
    /// already succeeded, so a failed commit is reported, never fatal.
    pub fn commit(&self, paths: &[String], message: &str) -> Option<String> {
        if !self.available || paths.is_empty() {
            return None;
        }
        let add = Command::new("git")
            .arg("add")
            .arg("--")
            .args(paths)
            .current_dir(&self.root)
            .output();
        if !add.is_ok_and(|out| out.status.success()) {
            return None;
        }
        let commit = Command::new("git")
            .arg("commit")
            // `--only -- <paths>` scopes the commit to the batch: the index
            // may hold unrelated pre-staged user work, and it must never be
            // swept into a `rusta:` commit (§6.9 commits edit batches only).
            .arg("--only")
            .arg("-m")
            .arg(message)
            .arg("--quiet")
            .arg("--")
            .args(paths)
            .current_dir(&self.root)
            .output();
        if !commit.is_ok_and(|out| out.status.success()) {
            return None; // e.g. nothing staged — not an error for the agent
        }
        self.head()
    }

    /// The current `HEAD` SHA, when git is available.
    pub fn head(&self) -> Option<String> {
        run(&self.root, ["rev-parse", "HEAD"])
            .ok()
            .map(|out| trim_newline(&out).to_owned())
    }

    /// The subject line of `HEAD` (first line of the commit message).
    pub fn head_message(&self) -> Option<String> {
        run(&self.root, ["log", "-1", "--pretty=%s"])
            .ok()
            .map(|out| trim_newline(&out).to_owned())
    }

    /// One-line-per-commit history (newest first), capped at `limit`.
    pub fn log(&self, limit: usize) -> Vec<String> {
        if !self.available {
            return Vec::new();
        }
        run(&self.root, ["log", &format!("-{limit}"), "--pretty=%h %s"])
            .ok()
            .map(|out| {
                out.lines()
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Reverts the commit `sha` **only if it is exactly `HEAD`** (§6.9).
    /// `git reset --mixed HEAD~1` moves HEAD and the index to the parent and
    /// leaves the working tree alone — the journal has already restored the
    /// file bytes, so the tree lands clean at the pre-batch state. Returns
    /// whether a reset actually happened.
    pub fn reset_if_head(&self, sha: &str) -> bool {
        if !self.available || self.head().as_deref() != Some(sha) {
            return false; // someone committed after us — never touch it
        }
        Command::new("git")
            .args(["reset", "--mixed", "HEAD~1", "--quiet"])
            .current_dir(&self.root)
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// The `/diff` body: the most recent rusta batch (`git show HEAD`) when
    /// HEAD is ours, otherwise the uncommitted working-tree diff. Capped and
    /// annotated when truncated.
    pub fn diff(&self) -> String {
        if !self.available {
            return "not a git repository — no diff available".to_owned();
        }
        let ours = self
            .head_message()
            .is_some_and(|message| message.starts_with(COMMIT_PREFIX));
        let args: [&str; 4] = if ours {
            ["show", "--stat", "--patch", "HEAD"]
        } else {
            ["diff", "--stat", "--patch", "HEAD"]
        };
        match run(&self.root, args) {
            Ok(out) => {
                let total = out.lines().count();
                if total <= DIFF_MAX_LINES {
                    return out;
                }
                let mut capped: Vec<&str> = out.lines().take(DIFF_MAX_LINES).collect();
                let note = format!("… [truncated {} lines]", total - DIFF_MAX_LINES);
                capped.push(&note);
                capped.join("\n")
            }
            Err(_) => "git diff failed — check the repository state".to_owned(),
        }
    }

    /// True when `HEAD` is exactly `sha` (used by `/undo` reporting).
    pub fn head_is(&self, sha: &str) -> bool {
        self.head().as_deref() == Some(sha)
    }
}

/// Runs a sync git subcommand, capturing stdout as lossy UTF-8.
fn run(root: &Path, args: impl IntoIterator<Item = impl Into<String>>) -> Result<String, String> {
    let mut command = Command::new("git");
    for arg in args {
        command.arg(arg.into());
    }
    let output = command
        .current_dir(root)
        .output()
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).into_owned())
    }
}

fn trim_newline(text: &str) -> &str {
    text.trim_end_matches(['\r', '\n'])
}

/// Async wrapper for the batch commit on the agent's hot path (§6.9) — the
/// same semantics as [`Git::commit`], run on the blocking pool so backend
/// work overlaps cleanly.
pub async fn commit_batch(git: &Git, paths: &[String], summary: &str) -> Option<String> {
    if !git.available() {
        return None;
    }
    let root = git.toplevel();
    let paths = paths.to_vec();
    let message = format!("{COMMIT_PREFIX}{summary}");
    tokio::task::spawn_blocking(move || Git::open(root).commit(&paths, &message))
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_present() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let ok = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("spawn git")
                .status
                .success()
        };
        assert!(git_present(), "git required for git.rs tests");
        assert!(ok(&["init", "--quiet"]));
        assert!(ok(&["config", "user.email", "rusta@test"]));
        assert!(ok(&["config", "user.name", "Rusta Test"]));
        dir
    }

    #[test]
    fn commit_and_reset_round_trip() {
        let dir = init_repo();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "one\n").expect("write");

        let git = Git::open(root);
        assert!(git.available());
        assert_eq!(git.toplevel(), root.canonicalize().expect("canonical"));

        let sha = git
            .commit(&["a.txt".to_owned()], "rusta: first batch")
            .expect("commit");
        assert!(git.head_is(&sha));
        assert_eq!(
            git.head_message().as_deref(),
            Some("rusta: first batch"),
            "§6.9 message contract"
        );
        assert_eq!(git.log(5).len(), 1);

        // An unrelated commit lands after ours — /undo must not touch it.
        std::fs::write(root.join("b.txt"), "b\n").expect("write");
        run(root, ["add", "."]).expect("add");
        Command::new("git")
            .args(["commit", "-m", "user's own commit", "--quiet"])
            .current_dir(root)
            .output()
            .expect("commit");
        let user_sha = git.head().expect("user sha");
        assert!(!git.reset_if_head(&sha), "unrelated HEAD is never reset");

        // When HEAD *is* ours, the reset lands on exactly its parent.
        std::fs::write(root.join("a.txt"), "two\n").expect("write");
        let sha2 = git
            .commit(&["a.txt".to_owned()], "rusta: second batch")
            .expect("commit 2");
        assert!(git.reset_if_head(&sha2));
        assert_eq!(
            git.head().as_deref(),
            Some(user_sha.as_str()),
            "parent restored"
        );
    }

    #[test]
    fn commit_scopes_to_the_batch_not_the_index() {
        let dir = init_repo();
        let root = dir.path();
        // Unrelated user work, staged before the agent's edit lands.
        std::fs::write(root.join("staged.txt"), "user work\n").expect("write");
        assert!(
            Command::new("git")
                .args(["add", "staged.txt"])
                .current_dir(root)
                .output()
                .expect("stage")
                .status
                .success()
        );
        std::fs::write(root.join("a.txt"), "one\n").expect("write");

        let git = Git::open(root);
        let sha = git
            .commit(&["a.txt".to_owned()], "rusta: batch")
            .expect("commit");

        // The commit contains exactly the batch path …
        let names = run(
            root,
            ["show", "--name-only", "--pretty=format:", sha.as_str()],
        )
        .expect("show");
        assert_eq!(
            names.split_whitespace().collect::<Vec<_>>(),
            vec!["a.txt"],
            "{names}"
        );
        // … and the user's staged work is untouched — still staged.
        let status = run(root, ["status", "--porcelain"]).expect("status");
        assert!(
            status.lines().any(|line| line.starts_with("A  staged.txt")),
            "{status}"
        );
    }

    #[test]
    fn absent_repo_degrades_to_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = Git::open(dir.path());
        assert!(!git.available());
        assert!(git.commit(&["x".to_owned()], "rusta: x").is_none());
        assert!(git.head().is_none());
        assert!(!git.reset_if_head("deadbeef"));
        assert!(git.log(3).is_empty());
        assert!(git.diff().contains("not a git repository"));
    }

    #[tokio::test]
    async fn async_commit_batch_matches_sync() {
        if !git_present() {
            return; // environments without git skip gracefully
        }
        let dir = init_repo();
        std::fs::write(dir.path().join("c.txt"), "c\n").expect("write");
        let git = Git::open(dir.path());
        let sha = commit_batch(&git, &["c.txt".to_owned()], "async batch")
            .await
            .expect("commit");
        assert!(git.head_is(&sha));
        assert_eq!(git.head_message().as_deref(), Some("rusta: async batch"));
        assert!(git.diff().lines().count() > 0);
    }
}
