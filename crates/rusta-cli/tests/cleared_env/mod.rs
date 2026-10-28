//! What a test that runs `rusta` with a cleared environment gives back.

use std::process::Command;

pub trait ClearedEnv {
    /// Set `path`, a Unix `PATH`, as the `PATH` of this command, whose
    /// environment is cleared; on Windows, also give back what a process
    /// needs there: `SystemRoot`, without which it cannot open a socket, and
    /// `TEMP` and `TMP`. A Unix `PATH` names no Windows folder, so there a
    /// non-empty `path` stands for the test's own `PATH`, where Git's `sh` is
    /// found.
    fn cleared_env_path(&mut self, path: &str) -> &mut Self;
}

impl ClearedEnv for Command {
    fn cleared_env_path(&mut self, path: &str) -> &mut Self {
        if cfg!(windows) {
            for name in ["SystemRoot", "TEMP", "TMP"] {
                if let Some(value) = std::env::var_os(name) {
                    self.env(name, value);
                }
            }
            if !path.is_empty() {
                return self.env("PATH", std::env::var_os("PATH").unwrap_or_default());
            }
        }
        self.env("PATH", path)
    }
}
