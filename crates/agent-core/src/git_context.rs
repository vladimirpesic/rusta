//! The git-context prefix of the `explore` agent: branch, dirty files and
//! recent commits, so a read-only explorer orients itself before searching.
//!
//! Nothing is prepended when there is no repository to describe: a block
//! saying so would only tell the model what it cannot know.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Per-command ceiling: a slow or hanging `git` must not stall the spawn of a
/// sub-agent.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Dirty files shown in full; beyond this only a count remains.
const MAX_DIRTY_FILES: usize = 20;
/// Commit lines are truncated to this length.
const MAX_COMMIT_LINE_CHARS: usize = 200;

/// Hosts whose remote URL may be named in the context; anything else is
/// dropped so a self-hosted remote (and any credentials in its URL) stays
/// out of the model's reach.
const ALLOWED_HOSTS: [&str; 6] = [
    "github.com",
    "gitlab.com",
    "gitee.com",
    "bitbucket.org",
    "codeberg.org",
    "git.sr.ht",
];

/// Collect the git context for `cwd`, or an empty string when `cwd` is not
/// inside a work tree (or git itself is unavailable or too slow).
pub async fn collect(cwd: &Path) -> String {
    let rev_parse = match run_git(cwd, &["rev-parse", "--is-inside-work-tree"]).await {
        Ok(output) => output,
        Err(_) => return String::new(),
    };
    if !rev_parse.status.success() {
        return String::new();
    }

    let (remote, branch, status, log) = tokio::join!(
        run_git(cwd, &["remote", "get-url", "origin"]),
        run_git(cwd, &["symbolic-ref", "--short", "HEAD"]),
        run_git(cwd, &["status", "--porcelain"]),
        run_git(cwd, &["log", "-3", "--format=%h %s"]),
    );
    let stdout = |result: &std::io::Result<std::process::Output>| {
        result
            .as_ref()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
            .unwrap_or_default()
    };

    let mut sections: Vec<String> = vec![format!("Working directory: {}", cwd.display())];

    let remote_url = stdout(&remote);
    if !remote_url.is_empty()
        && let Some(safe_url) = sanitize_remote_url(&remote_url)
    {
        sections.push(format!("Remote: {safe_url}"));
        if let Some(project) = project_name(&safe_url) {
            sections.push(format!("Project: {project}"));
        }
    }

    let branch_name = stdout(&branch);
    if !branch_name.is_empty() {
        sections.push(format!("Branch: {branch_name}"));
    }

    let dirty_text = stdout(&status);
    let dirty: Vec<&str> = dirty_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if !dirty.is_empty() {
        let shown = dirty.len().min(MAX_DIRTY_FILES);
        let mut body: String = dirty[..shown]
            .iter()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        if dirty.len() > shown {
            body.push_str(&format!("\n  ... and {} more", dirty.len() - shown));
        }
        sections.push(format!("Dirty files ({}):\n{body}", dirty.len()));
    }

    let log_lines: Vec<String> = stdout(&log)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let cut = line
                .char_indices()
                .nth(MAX_COMMIT_LINE_CHARS)
                .map(|(idx, _)| idx);
            match cut {
                Some(idx) => format!("  {}", &line[..idx]),
                None => format!("  {line}"),
            }
        })
        .collect();
    if !log_lines.is_empty() {
        sections.push(format!("Recent commits:\n{}", log_lines.join("\n")));
    }

    if sections.len() <= 1 {
        return String::new();
    }
    format!("<git-context>\n{}\n</git-context>", sections.join("\n"))
}

/// The name of git's executable.
const GIT: &str = if cfg!(windows) { "git.exe" } else { "git" };

/// Run one git command in `cwd`, capped at [`GIT_TIMEOUT`]. It runs unasked,
/// so it runs no program the folder chose: git comes from a `PATH` directory
/// outside the folder ([`git_outside`]), and the repository's
/// `core.fsmonitor` command, which `git status` would start, is off.
async fn run_git(cwd: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    let git = git_outside(std::env::var_os("PATH").as_deref().unwrap_or_default(), cwd)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no git on PATH"))?;
    let child = crate::child_env::command(git)
        .args(["-c", "core.fsmonitor=false", "-C"])
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    match tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output()).await {
        Ok(output) => output,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("git {} timed out", args.join(" ")),
        )),
    }
}

/// The first git that the `PATH` value `path` names in an absolute directory
/// outside `folder`: a `.` in `PATH`, or a directory of the folder's own, would
/// otherwise run whatever the folder calls `git`.
fn git_outside(path: &OsStr, folder: &Path) -> Option<PathBuf> {
    let resolved = |dir: &Path| std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let folder = resolved(folder);
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute() && !resolved(dir).starts_with(&folder))
        .map(|dir| dir.join(GIT))
        .find(|candidate| candidate.is_file())
}

/// The remote URL as it may appear in the context: scp-like URLs of known
/// hosts verbatim, http(s) URLs of known hosts normalized to https; anything
/// else — other schemes, other hosts — returns `None`.
fn sanitize_remote_url(remote_url: &str) -> Option<String> {
    for host in ALLOWED_HOSTS {
        if let Some(rest) = remote_url.strip_prefix(&format!("git@{host}:"))
            && !rest.is_empty()
        {
            return Some(remote_url.to_string());
        }
    }
    let parsed = url::Url::parse(remote_url).ok()?;
    if ALLOWED_HOSTS.contains(&parsed.host_str()?) {
        let port = parsed.port().map_or(String::new(), |p| format!(":{p}"));
        return Some(format!(
            "https://{}{}{}",
            parsed.host_str()?,
            port,
            parsed.path()
        ));
    }
    None
}

/// The `owner/repo` of a remote URL, sans `.git`.
fn project_name(remote_url: &str) -> Option<String> {
    // `/^[^/]+@[^/:]+:(.+)$/`: a scp-like `user@host:path`.
    let raw_path = match remote_url.split_once('@') {
        Some((user, rest)) if !user.contains('/') => match rest.split_once(':') {
            Some((host, path)) if !host.contains(['/', ':']) => path.to_string(),
            _ => url::Url::parse(remote_url).ok()?.path().to_string(),
        },
        _ => url::Url::parse(remote_url).ok()?.path().to_string(),
    };
    let trimmed = raw_path.trim_start_matches('/').trim_end_matches('/');
    let project = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    if project.is_empty() {
        None
    } else {
        Some(project.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_remote_url_keeps_known_hosts_only() {
        assert_eq!(
            sanitize_remote_url("git@github.com:acme/widgets.git"),
            Some("git@github.com:acme/widgets.git".to_string())
        );
        assert_eq!(
            sanitize_remote_url("https://gitlab.com/acme/widgets.git"),
            Some("https://gitlab.com/acme/widgets.git".to_string())
        );
        assert_eq!(
            sanitize_remote_url("ssh://git@github.com/acme/widgets.git"),
            Some("https://github.com/acme/widgets.git".to_string()),
            "any scheme of a known host normalizes to https"
        );
        assert_eq!(
            sanitize_remote_url("https://git.internal.example/acme/widgets.git"),
            None,
            "a self-hosted remote stays out of the context"
        );
        assert_eq!(sanitize_remote_url("not a url"), None);
    }

    #[test]
    fn project_name_parses_scp_and_urls() {
        assert_eq!(
            project_name("git@github.com:acme/widgets.git"),
            Some("acme/widgets".to_string())
        );
        assert_eq!(
            project_name("https://github.com/acme/widgets"),
            Some("acme/widgets".to_string())
        );
        assert_eq!(project_name("https://github.com/"), None);
    }

    /// Outside a repository the prefix is empty: nothing is prepended to the
    /// sub-agent's first message.
    #[tokio::test]
    async fn collect_is_empty_outside_a_repository() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(collect(temp.path()).await, "");
    }

    /// Inside a repository the prefix names the branch and the working
    /// directory.
    #[tokio::test]
    async fn collect_describes_a_repository() {
        let temp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .output()
        };
        if git(&["--version"]).is_err() {
            eprintln!("git unavailable; skipping");
            return;
        }
        for args in [
            &["init", "-b", "main"][..],
            &["config", "user.email", "t@example.com"][..],
            &["config", "user.name", "T"][..],
        ] {
            assert!(git(args).unwrap().status.success(), "git {args:?}");
        }
        std::fs::write(temp.path().join("file.txt"), "one").unwrap();
        git(&["add", "."]).unwrap();
        git(&["commit", "-m", "initial"]).unwrap();
        std::fs::write(temp.path().join("file.txt"), "two").unwrap();

        let context = collect(temp.path()).await;
        assert!(context.starts_with("<git-context>\n"), "{context}");
        assert!(context.contains("Working directory:"), "{context}");
        assert!(context.contains("Branch: main"), "{context}");
        assert!(context.contains("Dirty files (1):"), "{context}");
        assert!(context.contains("Recent commits:"), "{context}");
        assert!(context.contains("initial"), "{context}");
        assert!(context.ends_with("\n</git-context>"), "{context}");
    }

    /// The git that runs is one from a `PATH` directory outside the folder:
    /// not one of the folder's, nor one a relative entry finds.
    #[test]
    fn the_folders_own_git_is_not_run() {
        let folder = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let inside = folder.path().join("bin");
        std::fs::create_dir(&inside).unwrap();
        for dir in [folder.path(), inside.as_path(), elsewhere.path()] {
            std::fs::write(dir.join(GIT), "").unwrap();
        }
        let path = std::env::join_paths([
            PathBuf::from("."),
            inside.clone(),
            folder.path().to_path_buf(),
            elsewhere.path().to_path_buf(),
        ])
        .unwrap();
        assert_eq!(
            git_outside(&path, folder.path()),
            Some(elsewhere.path().join(GIT))
        );
        let none = std::env::join_paths([inside]).unwrap();
        assert_eq!(git_outside(&none, folder.path()), None);
    }

    /// A repository's `core.fsmonitor` command, which `git status` starts,
    /// does not run.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_repositorys_fsmonitor_command_does_not_run() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(temp.path())
                .output()
        };
        if git(&["--version"]).is_err() {
            eprintln!("git unavailable; skipping");
            return;
        }
        let ran = temp.path().join("ran");
        let hook = temp.path().join("monitor.sh");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", ran.display())).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(git(&["init", "-b", "main"]).unwrap().status.success());
        let hook = hook.to_string_lossy();
        assert!(
            git(&["config", "core.fsmonitor", &hook])
                .unwrap()
                .status
                .success()
        );
        // git itself runs it: the setting works.
        git(&["status", "--porcelain"]).unwrap();
        assert!(ran.exists(), "git status started core.fsmonitor");
        std::fs::remove_file(&ran).unwrap();

        collect(temp.path()).await;
        assert!(!ran.exists(), "the context's git status started it");
    }
}
