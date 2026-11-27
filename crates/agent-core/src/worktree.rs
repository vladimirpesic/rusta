//! A git worktree of its own for a sub-agent, and its work brought back.
//!
//! A sub-agent launched with `isolation: "worktree"` works in a copy of the
//! repository, so that sub-agents writing at once, or a sub-agent and its
//! parent, do not step on each other's files:
//!
//! - **Its copy** is the repository as it stands when it is launched,
//!   uncommitted and untracked files included (ignored ones left out): they are
//!   gathered in a temporary index, never the repository's own, into a commit
//!   that no branch holds, and that commit is checked out, detached, in a
//!   worktree under Rusta's data directory, with the repository's hooks off.
//!   The `node_modules`, `.venv` and `venv` folders at the repository's top are
//!   linked into it on Linux and macOS, so its tests can run.
//! - **Its work** comes back when it finishes, file by file: a file the working
//!   tree has not changed meanwhile takes the sub-agent's version; one both
//!   changed is merged three ways (`git merge-file`), and a conflict is left in
//!   the file with git's markers, for the parent to resolve. A file that cannot
//!   be merged — binary, or deleted on one side and changed on the other —
//!   keeps the working tree's version, and the sub-agent's is named by its
//!   commit (`git show <commit>:<path>`), which the ref
//!   `refs/rusta/agents/<id>/work` keeps until it is deleted
//!   (`git update-ref -d`). Nothing is staged or committed in the repository's
//!   branches.
//!
//! Work comes back one sub-agent at a time, and a file is replaced only if it is
//! still as it was read for the merge. A worktree left behind when Rusta
//! stopped while its sub-agent ran is removed, with git's record of it, once
//! it has gone untouched for [`STALE_AFTER`].

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::stale_guard::FileStamp;

/// The longest a git command may run.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);
/// The folders of installed dependencies a worktree is given, linked from the
/// repository's top, when git ignores them there.
const DEPENDENCY_DIRS: &[&str] = &["node_modules", ".venv", "venv"];
/// How long a worktree goes untouched before it counts as left behind.
const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Who the commits that hold a sub-agent's work are made as.
const AUTHOR: (&str, &str) = ("Rusta", "rusta@localhost");

/// One sub-agent at a time brings its work back.
static BRINGING_BACK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A sub-agent's worktree.
#[derive(Debug)]
pub struct Worktree {
    /// The top of the repository it is a copy of.
    top: PathBuf,
    /// Where the worktree is.
    path: PathBuf,
    /// The session's working directory within the repository, as a path
    /// relative to its top.
    within: PathBuf,
    /// The commit it was made from.
    base: String,
    /// The refs that keep its commits: `refs/rusta/agents/<id>/`.
    refs: String,
}

/// What became of a sub-agent's work when it was brought back.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BroughtBack {
    /// Files that took the sub-agent's version, its deletions included.
    pub taken: Vec<String>,
    /// Files both changed, merged cleanly.
    pub merged: Vec<String>,
    /// Files both changed, left with conflict markers.
    pub conflicted: Vec<String>,
    /// Files that kept the working tree's version, and the commit that holds
    /// the sub-agent's.
    pub kept: Vec<String>,
    /// The commit holding the sub-agent's work, when some of it did not come
    /// back cleanly.
    pub work: Option<String>,
}

impl BroughtBack {
    /// What it says to the parent, in a few lines.
    #[must_use]
    pub fn describe(&self) -> String {
        let list = |files: &[String]| {
            let mut shown: Vec<String> = files.iter().take(10).cloned().collect();
            if files.len() > 10 {
                shown.push(format!("and {} more", files.len() - 10));
            }
            shown.join(", ")
        };
        if self.taken.is_empty()
            && self.merged.is_empty()
            && self.conflicted.is_empty()
            && self.kept.is_empty()
        {
            return "It changed no file.".to_string();
        }
        let mut lines = Vec::new();
        if !self.taken.is_empty() {
            lines.push(format!(
                "Its changes are in the working tree: {}.",
                list(&self.taken)
            ));
        }
        if !self.merged.is_empty() {
            lines.push(format!(
                "Merged with changes made meanwhile: {}.",
                list(&self.merged)
            ));
        }
        if !self.conflicted.is_empty() {
            lines.push(format!(
                "Merged with conflicts, marked in the files for you to resolve: {}.",
                list(&self.conflicted)
            ));
        }
        if !self.kept.is_empty() {
            let work = self.work.as_deref().unwrap_or("its commit");
            lines.push(format!(
                "Kept as they were, its versions being in commit {work} (`git show \
                 {work}:<path>`): {}.",
                list(&self.kept)
            ));
        }
        lines.join("\n")
    }
}

impl Worktree {
    /// A worktree for the sub-agent `id` of a session working in `cwd`, made
    /// under `home` (`<data dir>/worktrees`).
    ///
    /// # Errors
    /// `cwd` is not in a git repository, or git fails.
    pub async fn create(cwd: &Path, home: &Path, id: &str) -> Result<Self, String> {
        let top = git_text(cwd, &["rev-parse", "--show-toplevel"], &[])
            .await
            .map_err(|e| {
                format!(
                    "isolation needs a git repository, and {} is not in one ({e})",
                    cwd.display()
                )
            })?;
        let top = PathBuf::from(top.trim());
        crate::private_fs::create_dir_all_async(home)
            .await
            .map_err(|e| format!("{}: {e}", home.display()))?;
        prune_left_behind(home).await;
        let within = PathBuf::from(
            git_text(cwd, &["rev-parse", "--show-prefix"], &[])
                .await?
                .trim(),
        );
        let path = home.join(id);
        let refs = format!("refs/rusta/agents/{id}");
        let base = snapshot(&top, &home.join(format!("{id}.base.index")), None).await?;
        git_text(&top, &["update-ref", &format!("{refs}/base"), &base], &[]).await?;
        let no_hooks = home.join("no-hooks");
        std::fs::create_dir_all(&no_hooks).map_err(|e| e.to_string())?;
        let hooks = format!("core.hooksPath={}", no_hooks.display());
        let path_text = path.display().to_string();
        git_text(
            &top,
            &[
                "-c", &hooks, "worktree", "add", "--detach", "--quiet", &path_text, &base,
            ],
            &[],
        )
        .await?;
        link_dependencies(&top, &path).await;
        Ok(Self {
            top,
            path,
            within,
            base,
            refs,
        })
    }

    /// Where the sub-agent works: the session's working directory, in the
    /// worktree.
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.path.join(&self.within)
    }

    /// Bring the sub-agent's work back into the repository's working tree,
    /// then remove the worktree.
    ///
    /// # Errors
    /// Git fails before anything is brought back; the worktree is kept then.
    pub async fn bring_back(self) -> Result<BroughtBack, String> {
        let _one_at_a_time = BRINGING_BACK.lock().await;
        let index = self
            .path
            .parent()
            .unwrap_or(&self.path)
            .join(format!("{}.work.index", self.refs.replace('/', "-")));
        let work = snapshot(&self.path, &index, Some(&self.base)).await?;
        let changes = git_bytes(
            &self.top,
            &[
                "diff",
                "--name-only",
                "--no-renames",
                "-z",
                &self.base,
                &work,
                "--",
            ],
            &[],
        )
        .await?;
        let mut brought = BroughtBack::default();
        for path in changes.split(|b| *b == 0).filter(|path| !path.is_empty()) {
            let path = String::from_utf8_lossy(path).into_owned();
            self.bring_file(&path, &work, &mut brought).await?;
        }
        let refs_kept = !brought.conflicted.is_empty() || !brought.kept.is_empty();
        if refs_kept {
            git_text(
                &self.top,
                &["update-ref", &format!("{}/work", self.refs), &work],
                &[],
            )
            .await?;
            brought.work = Some(work);
        } else {
            let _ = git_text(
                &self.top,
                &["update-ref", "-d", &format!("{}/base", self.refs)],
                &[],
            )
            .await;
        }
        self.remove().await;
        Ok(brought)
    }

    /// Bring one file back: `path` from the repository's top, changed by the
    /// sub-agent between the base and `work`.
    async fn bring_file(
        &self,
        path: &str,
        work: &str,
        brought: &mut BroughtBack,
    ) -> Result<(), String> {
        let base = blob(&self.top, &self.base, path).await?;
        let theirs = blob(&self.top, work, path).await?;
        let target = path
            .split('/')
            .fold(self.top.clone(), |joined, part| joined.join(part));
        if target.is_dir() {
            brought.kept.push(path.to_string());
            return Ok(());
        }
        for _ in 0..3 {
            let stamp = FileStamp::of(&target).await;
            let ours = match tokio::fs::read(&target).await {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("{path}: {e}")),
            };
            let outcome = if ours == base || ours == theirs {
                // Unchanged meanwhile, or changed the same way.
                if ours == theirs {
                    brought.taken.push(path.to_string());
                    return Ok(());
                }
                Outcome::Write(theirs.clone(), Taken::Theirs)
            } else {
                match (&ours, &base, &theirs) {
                    (Some(ours), Some(base), Some(theirs))
                        if !is_binary(ours) && !is_binary(base) && !is_binary(theirs) =>
                    {
                        let (merged, clean) = merge(&self.top, ours, base, theirs).await?;
                        Outcome::Write(
                            Some(merged),
                            if clean {
                                Taken::Merged
                            } else {
                                Taken::Conflicted
                            },
                        )
                    }
                    _ => Outcome::Keep,
                }
            };
            match outcome {
                Outcome::Keep => {
                    brought.kept.push(path.to_string());
                    return Ok(());
                }
                Outcome::Write(contents, taken) => {
                    let written = match contents {
                        Some(contents) => {
                            if let Some(parent) = target.parent() {
                                tokio::fs::create_dir_all(parent)
                                    .await
                                    .map_err(|e| format!("{path}: {e}"))?;
                            }
                            crate::replace_file::replace(&target, contents, stamp)
                                .await
                                .is_ok()
                        }
                        // Deleted by the sub-agent, unchanged here.
                        None => {
                            FileStamp::of(&target).await == stamp
                                && tokio::fs::remove_file(&target).await.is_ok()
                        }
                    };
                    if written {
                        if ours.is_none() {
                            copy_mode(&self.path.join(path), &target);
                        }
                        match taken {
                            Taken::Theirs => brought.taken.push(path.to_string()),
                            Taken::Merged => brought.merged.push(path.to_string()),
                            Taken::Conflicted => brought.conflicted.push(path.to_string()),
                        }
                        return Ok(());
                    }
                    // Changed while it was read: read again.
                }
            }
        }
        brought.kept.push(path.to_string());
        Ok(())
    }

    /// Remove the worktree and its refs without bringing anything back: for
    /// a sub-agent that never ran.
    pub async fn discard(self) {
        self.remove().await;
        let _ = git_text(
            &self.top,
            &["update-ref", "-d", &format!("{}/base", self.refs)],
            &[],
        )
        .await;
    }

    /// Remove the worktree and git's record of it.
    async fn remove(&self) {
        let path = self.path.display().to_string();
        if git_text(&self.top, &["worktree", "remove", "--force", &path], &[])
            .await
            .is_err()
        {
            let _ = std::fs::remove_dir_all(&self.path);
            let _ = git_text(&self.top, &["worktree", "prune"], &[]).await;
        }
    }
}

/// What to do with a file that comes back.
enum Outcome {
    /// Write these contents, or delete the file for `None`.
    Write(Option<Vec<u8>>, Taken),
    /// Keep the working tree's version.
    Keep,
}

/// How a written file came back.
#[derive(Clone, Copy)]
enum Taken {
    Theirs,
    Merged,
    Conflicted,
}

/// Remove the worktrees under `home` left untouched for [`STALE_AFTER`],
/// and the record their repositories keep of them.
async fn prune_left_behind(home: &Path) {
    let Ok(entries) = std::fs::read_dir(home) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE_AFTER);
        let Ok(link) = std::fs::read_to_string(path.join(".git")) else {
            continue;
        };
        if !stale {
            continue;
        }
        // `gitdir: <repository>/.git/worktrees/<name>`, whose repository
        // forgets it once the folder is gone.
        let repository = link
            .trim()
            .strip_prefix("gitdir:")
            .map(|dir| PathBuf::from(dir.trim()))
            .and_then(|dir| dir.parent().and_then(Path::parent).map(Path::to_path_buf));
        let _ = std::fs::remove_dir_all(&path);
        if let Some(repository) = repository {
            let git_dir = format!("--git-dir={}", repository.display());
            let _ = git_text(home, &[&git_dir, "worktree", "prune"], &[]).await;
        }
    }
}

/// A commit of the working tree at `dir`, uncommitted and untracked files
/// included, gathered in the temporary index `index`, whose parent is `parent`
/// or the checked-out commit (none in a repository without one).
async fn snapshot(dir: &Path, index: &Path, parent: Option<&str>) -> Result<String, String> {
    let index_text = index.display().to_string();
    let env = [("GIT_INDEX_FILE", index_text.as_str())];
    let head = match parent {
        Some(parent) => Some(parent.to_string()),
        None => git_text(dir, &["rev-parse", "--verify", "--quiet", "HEAD"], &[])
            .await
            .ok()
            .map(|head| head.trim().to_string())
            .filter(|head| !head.is_empty()),
    };
    let _ = std::fs::remove_file(index);
    match &head {
        Some(head) => git_text(dir, &["read-tree", head], &env).await?,
        None => git_text(dir, &["read-tree", "--empty"], &env).await?,
    };
    let result = async {
        git_text(dir, &["add", "--all", "--", "."], &env).await?;
        let tree = git_text(dir, &["write-tree"], &env).await?;
        let mut args = vec![
            "commit-tree",
            tree.trim(),
            "-m",
            "rusta: a sub-agent's worktree",
        ];
        if let Some(head) = &head {
            args.extend(["-p", head]);
        }
        let identity = [
            ("GIT_AUTHOR_NAME", AUTHOR.0),
            ("GIT_AUTHOR_EMAIL", AUTHOR.1),
            ("GIT_COMMITTER_NAME", AUTHOR.0),
            ("GIT_COMMITTER_EMAIL", AUTHOR.1),
        ];
        Ok::<_, String>(git_text(dir, &args, &identity).await?.trim().to_string())
    }
    .await;
    let _ = std::fs::remove_file(index);
    result
}

/// The blob of `path` at `commit`, or `None` where it has none.
async fn blob(top: &Path, commit: &str, path: &str) -> Result<Option<Vec<u8>>, String> {
    let spec = format!("{commit}:{path}");
    if git_bytes(top, &["cat-file", "-e", &spec], &[])
        .await
        .is_err()
    {
        return Ok(None);
    }
    git_bytes(top, &["cat-file", "blob", &spec], &[])
        .await
        .map(Some)
}

/// `ours` and `theirs` merged against `base` with `git merge-file`: the
/// result, conflicts marked, and whether it merged cleanly.
async fn merge(
    top: &Path,
    ours: &[u8],
    base: &[u8],
    theirs: &[u8],
) -> Result<(Vec<u8>, bool), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let file = |name: &str, bytes: &[u8]| -> Result<String, String> {
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(path.display().to_string())
    };
    let (ours, base, theirs) = (
        file("ours", ours)?,
        file("base", base)?,
        file("theirs", theirs)?,
    );
    let output = run_git(
        top,
        &[
            "merge-file",
            "-p",
            "-L",
            "working tree",
            "-L",
            "base",
            "-L",
            "sub-agent",
            &ours,
            &base,
            &theirs,
        ],
        &[],
    )
    .await?;
    // `merge-file` exits with the number of conflicts, negative on an error.
    match output.status.code() {
        Some(0) => Ok((output.stdout, true)),
        Some(code) if code > 0 => Ok((output.stdout, false)),
        _ => Err(format!(
            "git merge-file failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Whether `bytes` look binary to git: a NUL among the first 8,000.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|b| *b == 0)
}

/// Give a file the sub-agent created the permissions it had in the worktree,
/// its executable bit among them.
fn copy_mode(from: &Path, to: &Path) {
    #[cfg(unix)]
    if let Ok(metadata) = std::fs::metadata(from) {
        let _ = std::fs::set_permissions(to, metadata.permissions());
    }
    #[cfg(not(unix))]
    let _ = (from, to);
}

/// Link the repository's dependency folders into the worktree, where git
/// ignores them — a link it does not ignore would be taken for the
/// sub-agent's work — and the worktree has none.
async fn link_dependencies(top: &Path, worktree: &Path) {
    #[cfg(unix)]
    for name in DEPENDENCY_DIRS {
        let (source, link) = (top.join(name), worktree.join(name));
        if source.is_dir()
            && !link.exists()
            && git_bytes(top, &["check-ignore", "--quiet", name], &[])
                .await
                .is_ok()
        {
            let _ = std::os::unix::fs::symlink(&source, &link);
        }
    }
    #[cfg(not(unix))]
    let _ = (top, worktree, DEPENDENCY_DIRS);
}

/// Run git in `dir` with `args`, and `env` set, within [`GIT_TIMEOUT`].
async fn run_git(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<std::process::Output, String> {
    let mut command = crate::child_env::command("git");
    command
        .current_dir(dir)
        // A repository's config could otherwise run a program while git works.
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, value) in env {
        command.env(name, value);
    }
    let child = command
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("git {} took longer than {GIT_TIMEOUT:?}", args[0]))?
        .map_err(|e| format!("git {}: {e}", args[0]))
}

/// What git printed, when it succeeded.
async fn git_bytes(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<Vec<u8>, String> {
    let output = run_git(dir, args, env).await?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// [`git_bytes`] as text.
async fn git_text(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    git_bytes(dir, args, env)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "start"]);
        dir
    }

    fn read(dir: &Path, path: &str) -> String {
        std::fs::read_to_string(dir.join(path)).unwrap()
    }

    /// The worktree holds the repository as it stands, uncommitted and
    /// untracked files included and ignored ones left out; what the
    /// sub-agent changes comes back, and the repository's index and branches
    /// are untouched.
    #[tokio::test]
    async fn a_worktree_copies_the_working_tree_and_its_work_comes_back() {
        let repo = repository();
        let root = repo.path();
        std::fs::write(root.join("b.txt"), "b changed\n").unwrap();
        std::fs::write(root.join("new.txt"), "untracked\n").unwrap();
        std::fs::create_dir(root.join("ignored")).unwrap();
        std::fs::write(root.join("ignored").join("x"), "x").unwrap();
        let home = tempfile::tempdir().unwrap();

        let worktree = Worktree::create(root, home.path(), "sub-1").await.unwrap();
        let cwd = worktree.cwd();
        assert_eq!(read(&cwd, "b.txt"), "b changed\n");
        assert_eq!(read(&cwd, "new.txt"), "untracked\n");
        assert!(!cwd.join("ignored").exists());

        std::fs::write(cwd.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(cwd.join("made.txt"), "made\n").unwrap();
        std::fs::remove_file(cwd.join("new.txt")).unwrap();
        let brought = worktree.bring_back().await.unwrap();
        assert_eq!(
            brought,
            BroughtBack {
                taken: vec!["a.txt".into(), "made.txt".into(), "new.txt".into()],
                ..BroughtBack::default()
            }
        );
        assert_eq!(read(root, "a.txt"), "one\nTWO\nthree\n");
        assert_eq!(read(root, "made.txt"), "made\n");
        assert!(!root.join("new.txt").exists());
        assert!(!cwd.exists(), "the worktree is removed");

        let status = std::process::Command::new("git")
            .current_dir(root)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        let status = String::from_utf8(status.stdout).unwrap();
        assert!(
            status.contains(" M a.txt") && status.contains("?? made.txt"),
            "{status}"
        );
        let branches = std::process::Command::new("git")
            .current_dir(root)
            .args(["log", "--oneline", "--all"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(branches.stdout).unwrap().lines().count(),
            1
        );
    }

    /// A file changed on both sides is merged; a conflict is marked; a file
    /// that cannot be merged keeps the working tree's version, and the
    /// sub-agent's is named by its commit.
    #[tokio::test]
    async fn changes_on_both_sides_are_merged_or_marked() {
        let repo = repository();
        let root = repo.path();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        git(root, &["commit", "-q", "-am", "five lines"]);
        let home = tempfile::tempdir().unwrap();
        let worktree = Worktree::create(root, home.path(), "sub-2").await.unwrap();
        let cwd = worktree.cwd();

        std::fs::write(cwd.join("a.txt"), "ONE\ntwo\nthree\nfour\nfive\n").unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\nfour\nFIVE\n").unwrap();
        std::fs::write(cwd.join("b.txt"), "theirs\n").unwrap();
        std::fs::write(root.join("b.txt"), "ours\n").unwrap();
        std::fs::write(cwd.join("bin.dat"), [0u8, 1, 2]).unwrap();
        std::fs::write(root.join("bin.dat"), [0u8, 9]).unwrap();

        let brought = worktree.bring_back().await.unwrap();
        assert_eq!(brought.merged, ["a.txt"]);
        assert_eq!(brought.conflicted, ["b.txt"]);
        assert_eq!(brought.kept, ["bin.dat"]);
        assert_eq!(read(root, "a.txt"), "ONE\ntwo\nthree\nfour\nFIVE\n");
        let b = read(root, "b.txt");
        assert!(
            b.contains("<<<<<<< working tree") && b.contains(">>>>>>> sub-agent"),
            "{b}"
        );
        assert_eq!(std::fs::read(root.join("bin.dat")).unwrap(), [0u8, 9]);
        let work = brought.work.clone().unwrap();
        let theirs = std::process::Command::new("git")
            .current_dir(root)
            .args(["show", &format!("{work}:bin.dat")])
            .output()
            .unwrap();
        assert_eq!(theirs.stdout, [0u8, 1, 2]);
        assert!(
            brought.describe().contains("Merged with conflicts"),
            "{}",
            brought.describe()
        );
    }

    #[tokio::test]
    async fn outside_a_repository_isolation_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let error = Worktree::create(dir.path(), home.path(), "sub-3")
            .await
            .unwrap_err();
        assert!(
            error.starts_with("isolation needs a git repository"),
            "{error}"
        );
    }

    /// A worktree left untouched past [`STALE_AFTER`] is removed with its
    /// repository's record of it; a recent one stays.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_worktree_left_behind_is_pruned() {
        let repo = repository();
        let home = tempfile::tempdir().unwrap();
        let old = Worktree::create(repo.path(), home.path(), "sub-old")
            .await
            .unwrap();
        let recent = Worktree::create(repo.path(), home.path(), "sub-new")
            .await
            .unwrap();
        let long_ago = std::time::SystemTime::now() - STALE_AFTER - Duration::from_secs(60);
        std::fs::File::open(&old.path)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        prune_left_behind(home.path()).await;
        assert!(!old.path.exists());
        assert!(recent.path.exists());
        let list = std::process::Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "list"])
            .output()
            .unwrap();
        let list = String::from_utf8(list.stdout).unwrap();
        assert!(
            !list.contains("sub-old") && list.contains("sub-new"),
            "{list}"
        );
        recent.discard().await;
    }

    #[test]
    fn what_came_back_is_said_in_a_few_lines() {
        assert_eq!(BroughtBack::default().describe(), "It changed no file.");
        let brought = BroughtBack {
            taken: (0..12).map(|i| format!("f{i}")).collect(),
            ..BroughtBack::default()
        };
        assert!(
            brought.describe().ends_with("f9, and 2 more."),
            "{}",
            brought.describe()
        );
    }
}
