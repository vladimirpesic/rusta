//! The folders Rusta keeps the user's config, data and caches in, each under
//! a `rusta` folder of its own.
//!
//! `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `XDG_CACHE_HOME`, set to an
//! absolute path, name them on every platform. Otherwise they are the
//! platform's: `~/.config`, `~/.local/share` and `~/.cache` on Linux;
//! `~/Library/Application Support` for the config and data and
//! `~/Library/Caches` on macOS; the roaming `AppData` folder for the config
//! and data and the local one on Windows. Windows has no other way to move
//! them for one process, which is what keeps a test away from the user's own
//! config and keys.

use std::path::PathBuf;

/// The user config folder: `config.toml`, instruction files, profiles,
/// skills, MCP credentials.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    xdg("XDG_CONFIG_HOME").or_else(dirs::config_dir)
}

/// The data folder: sessions, history, trusted folders, background tasks,
/// spilled tool output and cached media.
#[must_use]
pub fn data_dir() -> Option<PathBuf> {
    xdg("XDG_DATA_HOME").or_else(dirs::data_dir)
}

/// The cache folder: model catalogs.
#[must_use]
pub fn cache_dir() -> Option<PathBuf> {
    xdg("XDG_CACHE_HOME").or_else(dirs::cache_dir)
}

/// The folder `var` names, when it is an absolute path: a relative one is
/// ignored, as the XDG base directory specification says.
fn xdg(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}
