use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use regex::Regex;
use tokio::fs;

use crate::errors::AgentError;

/// Matches the marker a record carries in place of a blob, capturing its id.
/// Ids are ULIDs; anything else, such as a path a crafted record might name,
/// is no marker at all.
pub(crate) static MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<blob id="([0-9A-Za-z]+)"/>"#).unwrap());

/// The marker a record carries in place of blob `id`'s content.
#[must_use]
pub fn marker(id: &str) -> String {
    format!("<blob id=\"{id}\"/>")
}

/// The ids of the blobs `text` refers to, in order, without repeats.
#[must_use]
pub fn referenced_ids(text: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for caps in MARKER_RE.captures_iter(text) {
        let id = &caps[1];
        if !ids.iter().any(|known| known == id) {
            ids.push(id.to_string());
        }
    }
    ids
}

/// On-disk sidecar store for large message payloads.
///
/// Large tool outputs are written to the blob directory and replaced in the
/// JSONL record file with a small `<blob id="..."/>` marker. The in-memory
/// cache lets `ContextMemory::build_history` expand markers synchronously.
#[derive(Debug)]
pub struct BlobStore {
    dir: PathBuf,
    cache: Mutex<HashMap<String, Vec<u8>>>,
}

impl BlobStore {
    /// Create or open a blob store rooted at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self, AgentError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)
            .map_err(|e| AgentError::Config(format!("failed to create blob dir: {e}")))?;
        Ok(Self {
            dir,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Write `content` to the store and return its unique id.
    pub async fn write(&self, content: impl AsRef<[u8]>) -> Result<String, AgentError> {
        let id = ulid::Ulid::generate().to_string();
        let bytes = content.as_ref().to_vec();
        fs::write(self.path(&id), &bytes)
            .await
            .map_err(|e| AgentError::Config(format!("failed to write blob {id}: {e}")))?;
        self.cache.lock().unwrap().insert(id.clone(), bytes);
        Ok(id)
    }

    /// Read a blob by id, consulting the in-memory cache first.
    pub fn read(&self, id: &str) -> Option<Vec<u8>> {
        if let Ok(guard) = self.cache.lock()
            && let Some(bytes) = guard.get(id).cloned()
        {
            return Some(bytes);
        }
        std::fs::read(self.path(id)).ok()
    }

    /// Return the filesystem path for a blob id.
    pub fn path(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Markers name blob ids, once each, in order; a marker whose id is not a
    /// plain name — a path a crafted record might carry — names nothing.
    #[test]
    fn referenced_ids_are_plain_names() {
        let text = format!(
            "{} and {} then {} and <blob id=\"../../etc/passwd\"/>",
            marker("01ABC"),
            marker("01DEF"),
            marker("01ABC")
        );
        assert_eq!(referenced_ids(&text), ["01ABC", "01DEF"]);
    }

    #[tokio::test]
    async fn test_write_and_read_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp.path().join("blobs")).unwrap();

        let id = store.write("large payload").await.unwrap();
        let bytes = store.read(&id).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "large payload");
    }

    #[tokio::test]
    async fn test_read_falls_back_to_disk() {
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::new(temp.path().join("blobs")).unwrap();
        let id = store.write("cached").await.unwrap();

        let store2 = BlobStore::new(temp.path().join("blobs")).unwrap();
        assert_eq!(
            String::from_utf8(store2.read(&id).unwrap()).unwrap(),
            "cached"
        );
    }
}
