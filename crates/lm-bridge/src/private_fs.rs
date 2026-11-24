//! Files and folders only their owner can read.
//!
//! What Rusta keeps — conversation records, blobs, spilled tool output, the
//! session index, task output, schedules, goals and other state — holds
//! prompts, source excerpts and command output. On Unix every folder made
//! for it here is `0700` and every file `0600`, whatever the umask, and one
//! that an earlier version left open to others is closed again when Rusta
//! next writes it. A file is replaced by writing a private temporary file
//! beside it and renaming that into place, so a crash leaves the old file or
//! the new one, never part of either.
//!
//! On Windows files inherit the access list of the folder they are made in;
//! Rusta's folders are in the user's profile (`%APPDATA%`, `%LOCALAPPDATA%`),
//! which only that user and administrators can read. Rusta sets no access
//! list of its own there.
//!
//! It lives here, in the crate every other Rusta crate builds on, so that the
//! providers' caches use it too; `agent_core::private_fs` is the same module.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// Make `path` and its missing parents, private, and make `path` itself
/// private if it already existed.
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        restrict(path, 0o700)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    Ok(())
}

/// Open `path` to append to, creating it private; a file that existed is
/// made private.
pub fn open_append(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    open_private(&mut options, path)
}

/// Create `path`, or empty it, private; a file that existed is made private.
pub fn create(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    open_private(&mut options, path)
}

/// Replace `path`'s contents with `contents`, atomically and privately: a
/// private temporary file in the same folder is written, flushed to disk and
/// renamed over `path`.
pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    // `tempfile` makes the file `0600` on Unix.
    let mut temporary = tempfile::Builder::new()
        .prefix(".rusta-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    temporary.write_all(contents.as_ref())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// [`create_dir_all`] off the async runtime.
pub async fn create_dir_all_async(path: &Path) -> io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || create_dir_all(&path))
        .await
        .map_err(io::Error::other)?
}

/// [`write`] off the async runtime.
pub async fn write_async(path: &Path, contents: impl Into<Vec<u8>>) -> io::Result<()> {
    let path = path.to_path_buf();
    let contents = contents.into();
    tokio::task::spawn_blocking(move || write(&path, contents))
        .await
        .map_err(io::Error::other)?
}

/// Copy `from` to `to`, privately.
pub async fn copy_async(from: &Path, to: &Path) -> io::Result<()> {
    let contents = tokio::fs::read(from).await?;
    write_async(to, contents).await
}

fn open_private(options: &mut std::fs::OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    restrict(path, 0o600)?;
    Ok(file)
}

/// Give `path` `mode`, when it has another.
#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path)?;
    if metadata.permissions().mode() & 0o7777 != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    /// Under the common umask, everything made here is the owner's alone, and
    /// what an earlier version left open is closed again.
    #[test]
    fn everything_made_here_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data/rusta/records");
        create_dir_all(&data).unwrap();
        assert_eq!(mode(&data), 0o700);
        assert_eq!(mode(&dir.path().join("data/rusta")), 0o700);

        let records = data.join("session.jsonl");
        open_append(&records).unwrap().write_all(b"{}\n").unwrap();
        assert_eq!(mode(&records), 0o600);

        let index = data.join("index.json");
        write(&index, "{}").unwrap();
        assert_eq!(mode(&index), 0o600);
        assert_eq!(std::fs::read_to_string(&index).unwrap(), "{}");

        let created = data.join("created");
        create(&created).unwrap();
        assert_eq!(mode(&created), 0o600);

        let old = dir.path().join("old");
        std::fs::create_dir(&old).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old_file = old.join("records.jsonl");
        std::fs::write(&old_file, "x").unwrap();
        std::fs::set_permissions(&old_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        create_dir_all(&old).unwrap();
        open_append(&old_file).unwrap();
        assert_eq!(mode(&old), 0o700);
        assert_eq!(mode(&old_file), 0o600);
    }

    /// A replacement that fails leaves the old file whole and no temporary
    /// file behind.
    #[test]
    fn a_failed_replacement_leaves_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        write(&path, "old").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = write(&path, "new");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        if refused.is_ok() {
            // Running as root: the folder's mode does not stop the write.
            return;
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
