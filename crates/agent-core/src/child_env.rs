//! The environment of the processes Rusta starts.
//!
//! Every process Rusta starts inherits its environment, except the variables
//! Rusta reads its own credentials from. That covers `bash` commands,
//! background tasks, hooks, MCP servers, `git`, and the system tools Rusta runs
//! itself. A command's output goes to the model and into the session records,
//! so a command that printed its environment would otherwise carry the key
//! with it. Nothing the model runs, and no MCP server, needs Rusta's key to do
//! its work.
//!
//! The names are registered once at startup with [`withhold`]. Every process is
//! built with [`command`] or [`std_command`], which start from Rusta's
//! environment without them. A variable set on the command afterwards, such as
//! an MCP server's own `env` entry, still reaches the process.
//!
//! Rusta's own environment still holds them, as it was started. On Linux,
//! [`close_own_environment`] keeps the processes it starts from reading it
//! there.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::sync::{LazyLock, PoisonError, RwLock};

static WITHHELD: LazyLock<RwLock<BTreeSet<String>>> = LazyLock::new(RwLock::default);

/// Keep the variables `names` out of every process started from now on.
/// Names add up: none is ever handed back. An empty name is ignored.
pub fn withhold<I>(names: I)
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    WITHHELD
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .extend(
            names
                .into_iter()
                .map(Into::into)
                .filter(|name| !name.is_empty()),
        );
}

/// The variables kept out of the processes Rusta starts, in order.
#[must_use]
pub fn withheld() -> Vec<String> {
    WITHHELD
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .cloned()
        .collect()
}

/// A command for `program` whose environment is Rusta's without the withheld
/// variables.
#[must_use]
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    for name in WITHHELD
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
    {
        command.env_remove(name);
    }
    command
}

/// [`std_command`], as a Tokio command.
#[must_use]
pub fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    std_command(program).into()
}

/// Keep the processes Rusta starts from reading its own environment, on
/// Linux. That environment is the one Rusta was started with, keys and all,
/// and `/proc/<pid>/environ` shows it to any process of the same user: a model
/// looking round its sandbox runs `tr '\0' '\n' < /proc/$PPID/environ`. Rusta's
/// process is made one the kernel does not dump (`PR_SET_DUMPABLE`), whose
/// `environ`, `mem` and open files only a process with the privilege to trace
/// any other can read. Its children are not affected: each process is
/// dumpable again from the program it runs.
///
/// Called once, at startup, before Rusta starts anything. Rusta then writes no
/// core dump, and a debugger attaches to it only with that privilege.
pub fn close_own_environment() {
    #[cfg(target_os = "linux")]
    if let Err(e) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        tracing::warn!("The processes Rusta starts can read its environment: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `command` does with `name`: `Some(None)` when it removes the
    /// variable, `Some(Some(value))` when it sets it, `None` when it inherits it.
    fn treatment(command: &std::process::Command, name: &str) -> Option<Option<String>> {
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new(name))
            .map(|(_, value)| value.map(|v| v.to_string_lossy().into_owned()))
    }

    /// A withheld variable is removed from a new command, and a variable the
    /// command is given afterwards, under the same name, still reaches it.
    #[test]
    fn a_withheld_variable_is_removed_and_one_given_afterwards_applies() {
        const NAME: &str = "RUSTA_TEST_CHILD_ENV_REMOVED";
        withhold([NAME]);

        assert_eq!(treatment(&std_command("true"), NAME), Some(None));
        assert_eq!(treatment(command("true").as_std(), NAME), Some(None));

        let mut given = std_command("true");
        given.env(NAME, "given");
        assert_eq!(treatment(&given, NAME), Some(Some("given".to_string())));
    }

    /// A variable nobody withheld is inherited as it is.
    #[test]
    fn other_variables_are_inherited() {
        assert_eq!(
            treatment(&std_command("true"), "RUSTA_TEST_CHILD_ENV_KEPT"),
            None
        );
    }

    /// A process Rusta starts reads Rusta's own environment from `/proc` until
    /// it is closed, and not after. Run in a process of its own, which closing
    /// changes for good. A superuser may read any process's, so the check
    /// needs another user.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_cannot_read_rustas_own_environment() {
        const IN_ITS_OWN_PROCESS: &str = "RUSTA_TEST_CLOSED_ENVIRONMENT";
        if std::env::var_os(IN_ITS_OWN_PROCESS).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "child_env::tests::a_child_cannot_read_rustas_own_environment",
                    "--nocapture",
                ])
                .env(IN_ITS_OWN_PROCESS, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        if rustix::process::geteuid().is_root() {
            return;
        }
        let environ = format!("/proc/{}/environ", std::process::id());
        let child_reads = || {
            let read = std_command("cat").arg(&environ).output().unwrap();
            read.status.success()
                && String::from_utf8_lossy(&read.stdout).contains(IN_ITS_OWN_PROCESS)
        };
        assert!(child_reads(), "a child reads an open environment");
        close_own_environment();
        assert!(!child_reads(), "a child read Rusta's closed environment");
    }

    /// Names registered at different times all stay withheld, and an empty name
    /// is never registered.
    #[test]
    fn names_add_up_and_empty_ones_are_ignored() {
        withhold(["RUSTA_TEST_CHILD_ENV_FIRST"]);
        withhold(vec![
            String::new(),
            "RUSTA_TEST_CHILD_ENV_SECOND".to_string(),
        ]);
        withhold(None::<String>);

        let withheld = withheld();
        assert!(withheld.iter().any(|n| n == "RUSTA_TEST_CHILD_ENV_FIRST"));
        assert!(withheld.iter().any(|n| n == "RUSTA_TEST_CHILD_ENV_SECOND"));
        assert!(!withheld.iter().any(String::is_empty));
    }
}
