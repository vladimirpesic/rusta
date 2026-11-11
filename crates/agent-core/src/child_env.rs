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
