//! A shell command and the processes it starts, stopped together.
//!
//! A command runs in a process group of its own on Unix, so stopping it stops
//! what it started too. Killed alone, the `sh` of `sh -c "npm run dev"` would
//! leave `node` running, still holding the port and the output pipes. On
//! Windows, which has no process groups, the tree is ended with
//! `taskkill /T`.
//!
//! Signals go through `kill(1)` rather than `libc`, so Rusta stays free of
//! `unsafe` code; `/bin/kill` is used where it exists, so a group can be
//! signalled whatever `PATH` holds. A group is named after `--`, as
//! `kill -TERM -- -<group>`: procps-ng 4.0.4's `kill` fails on a negative id
//! that follows the signal.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

/// How long a command asked to stop gets before it is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(2);

/// Start `cmd` in a process group of its own, which it leads: the group id is
/// its pid. Does nothing on Windows.
pub fn isolate(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Stop `child` and what it started: SIGTERM to its group, then SIGKILL to
/// what is left of the group once `grace` has passed. The shell that leads
/// the group usually exits at once, so it is the group, not the child, that
/// is waited for. The child is reaped. On Windows the tree is ended at once.
///
/// # Errors
/// Returns the error of waiting for the child.
pub async fn stop(child: &mut Child, grace: Duration) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    if let Some(group) = child.id() {
        signal(group, "TERM");
        let deadline = tokio::time::Instant::now() + grace;
        let status = tokio::time::timeout_at(deadline, child.wait()).await;
        while group_alive(group) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        signal(group, "KILL");
        if let Ok(status) = status {
            return status;
        }
    }
    #[cfg(not(unix))]
    let _ = grace;
    kill(child);
    child.wait().await
}

/// Kill `child` and what it started, at once and without waiting: for a
/// command that must end now. The child itself is killed whatever the
/// signal to its group could do.
pub fn kill(child: &mut Child) {
    if let Some(pid) = child.id() {
        kill_pid(pid);
    }
    let _ = child.start_kill();
}

/// Kill the command `pid` and what it started.
fn kill_pid(pid: u32) {
    #[cfg(unix)]
    signal(pid, "KILL");
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Kills a command and what it started when dropped, unless disarmed: for a
/// call whose future can be dropped while its command runs — a cancelled
/// turn — as `kill_on_drop` kills the shell alone.
pub struct KillOnDrop(Option<u32>);

impl KillOnDrop {
    #[must_use]
    pub fn new(child: &Child) -> Self {
        Self(child.id())
    }

    /// The command ended, or went to an owner of its own: leave what it
    /// started alone.
    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_pid(pid);
        }
    }
}

/// `kill(1)`, from `/bin` where it is there, with the signal `name` (`TERM`,
/// `KILL`, or `0` to test) for the process group `group`.
#[cfg(unix)]
fn kill_command(name: &str, group: u32) -> std::process::Command {
    let program = if std::path::Path::new("/bin/kill").exists() {
        "/bin/kill"
    } else {
        "kill"
    };
    let mut command = std::process::Command::new(program);
    command
        .arg(format!("-{name}"))
        .arg("--")
        .arg(format!("-{group}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Send `name` (`TERM`, `KILL`) to the process group `group`. A group that
/// is already gone is no error.
#[cfg(unix)]
fn signal(group: u32, name: &str) {
    if let Err(e) = kill_command(name, group).status() {
        tracing::warn!("could not SIG{name} process group {group}: {e}");
    }
}

/// Whether any process of the group `group` is still running.
#[cfg(unix)]
fn group_alive(group: u32) -> bool {
    kill_command("0", group)
        .status()
        .is_ok_and(|status| status.success())
}

/// What the tests of a command's processes need: a pid the command wrote to
/// a file, and whether a process is still running.
#[cfg(all(test, unix))]
pub(crate) mod testing {
    use std::time::Duration;

    /// Whether the system shows processes in `/proc`, where these checks look.
    pub(crate) fn can_see_processes() -> bool {
        std::path::Path::new("/proc/self/stat").exists()
    }

    /// The pid a command wrote to `file`, once it has.
    pub(crate) async fn pid_in(file: &std::path::Path) -> u32 {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(pid) = std::fs::read_to_string(file)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            assert!(tokio::time::Instant::now() < deadline, "no pid in {file:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Whether `pid` is a process still running, not a zombie.
    pub(crate) fn running(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit(')')
                    .next()
                    .map(|rest| rest.trim_start().to_owned())
            })
            .is_some_and(|rest| !rest.starts_with('Z'))
    }

    /// Kill the one process `pid`.
    pub(crate) fn kill_process(pid: u32) {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }

    /// Whether `pid` stops running within 30 s.
    pub(crate) async fn gone(pid: u32) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while running(pid) {
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        true
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::testing::{can_see_processes, gone, kill_process, pid_in, running};
    use super::*;

    /// `script` in a group of its own. Nothing it leaves behind holds the
    /// test's output open, should the test fail.
    fn command(script: &str) -> Child {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        isolate(&mut cmd);
        cmd.spawn().unwrap()
    }

    /// What a command started goes with it: on stop, and on kill.
    #[tokio::test]
    async fn the_processes_a_command_started_go_with_it() {
        if !can_see_processes() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for stopping in [true, false] {
            let file = dir.path().join(format!("pid-{stopping}"));
            let mut child = command(&format!("sleep 300 & echo $! > {}; wait", file.display()));
            let grandchild = pid_in(&file).await;
            assert!(running(grandchild));
            let ended = if stopping {
                tokio::time::timeout(Duration::from_secs(10), stop(&mut child, STOP_GRACE)).await
            } else {
                kill(&mut child);
                tokio::time::timeout(Duration::from_secs(10), child.wait()).await
            };
            assert!(ended.is_ok(), "the command did not end");
            assert!(gone(grandchild).await, "the sleep outlived its command");
        }
    }

    /// A process that ignores SIGTERM is killed once the grace is over.
    #[tokio::test]
    async fn a_process_deaf_to_sigterm_is_killed_after_the_grace() {
        if !can_see_processes() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pid");
        let mut child = command(&format!(
            "sh -c 'trap \"\" TERM; echo $$ > {}; i=0; while [ $i -lt 600 ]; do sleep 1; i=$((i+1)); done' & wait",
            file.display()
        ));
        let deaf = pid_in(&file).await;
        let ended = tokio::time::timeout(
            Duration::from_secs(10),
            stop(&mut child, Duration::from_millis(300)),
        )
        .await;
        assert!(ended.is_ok(), "the command did not end");
        assert!(gone(deaf).await, "the TERM-deaf process survived");
    }

    /// A guard disarmed leaves the group be; one dropped armed kills it.
    #[tokio::test]
    async fn a_dropped_guard_kills_and_a_disarmed_one_does_not() {
        if !can_see_processes() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        for armed in [true, false] {
            let file = dir.path().join(format!("pid-{armed}"));
            let mut child = command(&format!(
                "sleep 300 > /dev/null 2>&1 & echo $! > {}",
                file.display()
            ));
            let grandchild = pid_in(&file).await;
            let mut guard = KillOnDrop::new(&child);
            child.wait().await.unwrap();
            if !armed {
                guard.disarm();
            }
            drop(guard);
            if armed {
                assert!(gone(grandchild).await, "an armed guard kills the group");
            } else {
                assert!(running(grandchild), "a disarmed guard leaves it");
                kill_process(grandchild);
            }
        }
    }
}
