//! Replace a file's contents without ever leaving it half-written.
//!
//! `write_file` and `edit_file` write the new contents to a temporary file in
//! the file's own folder, with the file's own permissions, flush it to disk,
//! check that the file is still what the tool read, and rename the temporary
//! file over it. A failure at any point — a full disk, an I/O error, a crash
//! — leaves the old contents whole; a file changed by someone else since the
//! tool read it is left alone and the write refused. A new file gets the
//! permissions any new file gets (the umask's).
//!
//! A symlink is replaced through: its target gets the new contents, and the
//! link stays a link. A file with other hard links, or owned by another user,
//! is written in place instead (after the same check), because a rename
//! would give it a new identity: the other links would keep the old
//! contents, and the file would change owners. So is a file whose folder
//! does not let Rusta create the temporary file. Renaming also drops extended
//! attributes and access lists set on the old file.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::stale_guard::FileStamp;

/// Why a replacement did not happen.
#[derive(Debug)]
pub enum ReplaceError {
    /// The file is no longer what the tool read: someone changed it.
    Changed,
    /// Reading, writing or renaming failed.
    Io(io::Error),
}

impl ReplaceError {
    /// The message handed to the model.
    #[must_use]
    pub fn message(&self, path: &Path) -> String {
        match self {
            Self::Changed => format!(
                "Refusing to write {}: the file changed on disk while it was being written, so \
                 the new content is based on an older version. Read it again with read_file, \
                 then retry.",
                path.display()
            ),
            Self::Io(e) => format!("Failed to write {}: {e}", path.display()),
        }
    }
}

impl From<io::Error> for ReplaceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Replace `path`'s contents with `contents`, as long as the file still is
/// `expected`: as the tool read it, or absent when `expected` is `None`.
pub async fn replace(
    path: &Path,
    contents: Vec<u8>,
    expected: Option<FileStamp>,
) -> Result<(), ReplaceError> {
    replace_with(path, expected, move |file| file.write_all(&contents)).await
}

/// [`replace`], with `write` filling the temporary file.
async fn replace_with(
    path: &Path,
    expected: Option<FileStamp>,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()> + Send + 'static,
) -> Result<(), ReplaceError> {
    let target = path.to_path_buf();
    let prepared = tokio::task::spawn_blocking(move || prepare(&target, write))
        .await
        .map_err(io::Error::other)??;
    // As late as possible: the file must still be what the tool read.
    if FileStamp::of(path).await != expected {
        return Err(ReplaceError::Changed);
    }
    tokio::task::spawn_blocking(move || prepared.commit())
        .await
        .map_err(io::Error::other)??;
    Ok(())
}

/// The new contents, ready to take the file's place.
enum Prepared {
    /// A temporary file beside the target, renamed over it on commit.
    Renamed {
        temporary: tempfile::NamedTempFile,
        target: PathBuf,
    },
    /// Written over the target on commit: the bytes the writer produced.
    InPlace { target: PathBuf, contents: Vec<u8> },
}

impl Prepared {
    fn commit(self) -> io::Result<()> {
        match self {
            Self::Renamed { temporary, target } => {
                temporary.persist(&target).map_err(|e| e.error)?;
                sync_parent(&target);
                Ok(())
            }
            Self::InPlace { target, contents } => {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&target)?;
                file.write_all(&contents)?;
                file.sync_all()
            }
        }
    }
}

fn prepare(
    path: &Path,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<Prepared> {
    // A symlink's target is what gets replaced.
    let target = if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::canonicalize(path)?
    } else {
        path.to_path_buf()
    };
    let existing = std::fs::metadata(&target).ok();
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let mut builder = tempfile::Builder::new();
    builder.prefix(".rusta-").suffix(".tmp");
    #[cfg(unix)]
    if existing.is_none() {
        // A new file gets the permissions any new file gets: 0666 less the umask.
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let mut temporary = match builder.tempfile_in(dir) {
        Ok(temporary) => temporary,
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied && existing.is_some() => {
            return in_place(target, write);
        }
        Err(e) => return Err(e),
    };
    if let Some(metadata) = &existing {
        if keeps_identity(metadata, temporary.as_file()) {
            drop(temporary);
            return in_place(target, write);
        }
        std::fs::set_permissions(temporary.path(), metadata.permissions())?;
    }
    write(temporary.as_file_mut())?;
    temporary.as_file().sync_all()?;
    Ok(Prepared::Renamed { temporary, target })
}

/// The writer's bytes, to be written over `target` in place.
fn in_place(
    target: PathBuf,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<Prepared> {
    let mut contents = Vec::new();
    write(&mut contents)?;
    Ok(Prepared::InPlace { target, contents })
}

/// Whether a rename would change the file's identity: it has other hard
/// links, or another owner than the files Rusta makes (`made`).
#[cfg(unix)]
fn keeps_identity(existing: &std::fs::Metadata, made: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    existing.nlink() > 1
        || made
            .metadata()
            .is_ok_and(|made| made.uid() != existing.uid())
}

#[cfg(not(unix))]
fn keeps_identity(_existing: &std::fs::Metadata, _made: &std::fs::File) -> bool {
    false
}

/// Flush the folder holding `path`, so that the rename itself survives a
/// crash. Best effort: not every platform or filesystem allows it.
fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent()
        && let Ok(dir) = std::fs::File::open(dir)
    {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn stamp(path: &Path) -> Option<FileStamp> {
        FileStamp::of(path).await
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".rusta-"))
            .collect()
    }

    #[tokio::test]
    async fn a_file_is_replaced_whole_and_keeps_its_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.sh");
        std::fs::write(&path, "old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        }
        let expected = stamp(&path).await;
        replace(&path, b"new\n".to_vec(), expected).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o751);
        }
        assert!(leftovers(dir.path()).is_empty());
    }

    /// A new file gets what any new file gets, not the temporary file's 0600.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_new_file_gets_the_usual_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.txt");
        replace(&path, b"x".to_vec(), None).await.unwrap();
        let reference = dir.path().join("reference.txt");
        std::fs::write(&reference, "x").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&path), mode(&reference));
    }

    /// A file someone changed since it was read is left as they left it.
    #[tokio::test]
    async fn a_file_changed_since_it_was_read_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        std::fs::write(&path, "fn a() {}\n").unwrap();
        let expected = stamp(&path).await;
        std::fs::write(&path, "fn b() {}\n").unwrap();
        let refused = replace(&path, b"fn c() {}\n".to_vec(), expected).await;
        assert!(matches!(refused, Err(ReplaceError::Changed)));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fn b() {}\n");
        assert!(leftovers(dir.path()).is_empty());

        // A file created where none was is not overwritten either.
        let created = dir.path().join("created.rs");
        std::fs::write(&created, "theirs").unwrap();
        let refused = replace(&created, b"ours".to_vec(), None).await;
        assert!(matches!(refused, Err(ReplaceError::Changed)));
        assert_eq!(std::fs::read_to_string(&created).unwrap(), "theirs");
    }

    /// A write that fails part-way — a full disk, say — leaves the old
    /// contents and no temporary file.
    #[tokio::test]
    async fn a_write_that_fails_part_way_leaves_the_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.txt");
        std::fs::write(&path, "intact").unwrap();
        let expected = stamp(&path).await;
        let failed = replace_with(&path, expected, |file| {
            file.write_all(b"half")?;
            Err(io::Error::other("No space left on device"))
        })
        .await;
        assert!(matches!(failed, Err(ReplaceError::Io(_))));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "intact");
        assert!(leftovers(dir.path()).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_stays_a_link_and_its_target_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.txt");
        let link = dir.path().join("link.txt");
        std::fs::write(&target, "old").unwrap();
        std::os::unix::fs::symlink("real.txt", &link).unwrap();
        let expected = stamp(&link).await;
        replace(&link, b"new".to_vec(), expected).await.unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    /// A file with another hard link is written in place, so both names see
    /// the new contents.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hard_linked_file_is_written_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let other = dir.path().join("b.txt");
        std::fs::write(&path, "old").unwrap();
        std::fs::hard_link(&path, &other).unwrap();
        let expected = stamp(&path).await;
        replace(&path, b"new".to_vec(), expected).await.unwrap();
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "new");
    }

    /// A folder that refuses the temporary file still lets a writable file
    /// be written, in place.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_locked_folder_falls_back_to_writing_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let path = locked.join("config.txt");
        std::fs::write(&path, "old").unwrap();
        let expected = stamp(&path).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = replace(&path, b"new".to_vec(), expected).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        result.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }
}
