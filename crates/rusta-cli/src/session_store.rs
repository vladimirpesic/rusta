use std::path::PathBuf;

use agent_core::records::{Records, SESSION_SIDECARS, sidecar_path};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Metadata for a persisted conversation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    pub display_title: String,
    pub records_path: PathBuf,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    pub provider: String,
    pub model: String,
    /// The working directory the session ran in. `None` for entries written
    /// before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

impl SessionEntry {
    /// The title, or the id when the session has none.
    #[must_use]
    pub fn label(&self) -> &str {
        if self.display_title.trim().is_empty() {
            &self.id
        } else {
            &self.display_title
        }
    }
}

/// What [`SessionStore::record_activity`] records.
pub struct Activity {
    pub id: String,
    /// A new title, set with `/title`; `None` keeps the one on record, such
    /// as a fork's.
    pub display_title: Option<String>,
    pub records_path: PathBuf,
    pub provider: String,
    pub model: String,
    pub cwd: PathBuf,
}

/// On-disk index of conversation sessions.
pub struct SessionStore {
    path: PathBuf,
    entries: Vec<SessionEntry>,
}

impl SessionStore {
    /// Open (or create) a session index stored next to the records directory.
    pub async fn new(records_dir: PathBuf) -> Self {
        std::fs::create_dir_all(&records_dir).ok();
        let path = records_dir.join("index.json");
        let entries = read_entries(&path).await;
        Self { path, entries }
    }

    /// Return all sessions sorted by most-recent activity first.
    pub fn list(&self) -> Vec<SessionEntry> {
        let mut entries = self.entries.clone();
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_active_at));
        entries
    }

    /// Look up a session by id.
    pub fn get(&self, id: &str) -> Option<SessionEntry> {
        self.entries.iter().find(|e| e.id == id).cloned()
    }

    /// Insert or update a session and persist the index.
    pub async fn upsert(&mut self, entry: SessionEntry) -> Result<(), String> {
        self.update(|entries| put(entries, entry)).await
    }

    /// Record activity for a session, preserving its original `created_at`
    /// and, unless the activity sets a new one, its title.
    pub async fn record_activity(&mut self, activity: Activity) -> Result<(), String> {
        self.update(|entries| {
            let now = Utc::now();
            let existing = entries.iter().find(|e| e.id == activity.id);
            let created_at = existing.map_or(now, |e| e.created_at);
            let display_title = activity
                .display_title
                .or_else(|| existing.map(|e| e.display_title.clone()))
                .unwrap_or_default();
            put(
                entries,
                SessionEntry {
                    id: activity.id,
                    display_title,
                    records_path: activity.records_path,
                    created_at,
                    last_active_at: now,
                    provider: activity.provider,
                    model: activity.model,
                    cwd: Some(activity.cwd),
                },
            );
        })
        .await
    }

    /// Give session `id` the title `title` if it still has none: one set in
    /// the meantime, with `/title` or by another process, stays. Whether the
    /// title was set.
    pub async fn title_if_untitled(&mut self, id: &str, title: &str) -> Result<bool, String> {
        self.update(|entries| {
            let Some(entry) = entries.iter_mut().find(|e| e.id == id) else {
                return false;
            };
            if !entry.display_title.trim().is_empty() {
                return false;
            }
            entry.display_title = title.to_string();
            true
        })
        .await
    }

    /// Apply `change` to the index as it is on disk now, and save it. Read
    /// when the store opened, the index lost what other Rusta processes
    /// added since — a session started in another terminal vanished from
    /// the list at this one's next save. A short lock beside the index
    /// keeps two writers from interleaving ([`IndexLock`]).
    async fn update<R>(
        &mut self,
        change: impl FnOnce(&mut Vec<SessionEntry>) -> R,
    ) -> Result<R, String> {
        let _lock = IndexLock::acquire(&self.path).await;
        self.entries = read_entries(&self.path).await;
        let result = change(&mut self.entries);
        self.save().await?;
        Ok(result)
    }

    /// Copy session `source` into a new session `id`: its records file is
    /// copied next to the original, with the session state kept beside it
    /// ([`SESSION_SIDECARS`]), and the index gains an entry for it. The
    /// original is left as it was. Blob sidecars are shared by every session
    /// in the records directory, so they need no copy.
    pub async fn fork(&mut self, source: &str, id: &str) -> Result<SessionEntry, String> {
        let original = self
            .get(source)
            .ok_or_else(|| format!("no session '{source}'"))?;
        if self.get(id).is_some() {
            return Err(format!("session '{id}' already exists"));
        }
        let target = original.records_path.with_file_name(format!("{id}.jsonl"));
        if tokio::fs::try_exists(&target).await.unwrap_or(false) {
            return Err(format!("{} already exists", target.display()));
        }
        tokio::fs::copy(&original.records_path, &target)
            .await
            .map_err(|e| {
                format!(
                    "failed to copy {} to {}: {e}",
                    original.records_path.display(),
                    target.display()
                )
            })?;
        // A fork is resumed like the original would be, so what a resume
        // loads travels with it. Each sidecar is optional. Cron tasks do not
        // travel: they schedule prompts into *this* session, so a fork would
        // double-fire the parent's reminders. A fork of a session that has
        // tasks is told they stayed behind, and that it can make its own.
        let source_cron = sidecar_path(&original.records_path, "cron");
        if agent_core::cron::CronManager::load(&source_cron, true).task_count() > 0 {
            Records::new(&target)
                .append(&agent_core::cron::cron_fork_cleared_message())
                .await
                .map_err(|e| format!("failed to write {}: {e}", target.display()))?;
        }
        for kind in SESSION_SIDECARS {
            if kind == "cron" {
                continue;
            }
            let from = sidecar_path(&original.records_path, kind);
            if !tokio::fs::try_exists(&from).await.unwrap_or(false) {
                continue;
            }
            let to = sidecar_path(&target, kind);
            tokio::fs::copy(&from, &to).await.map_err(|e| {
                format!("failed to copy {} to {}: {e}", from.display(), to.display())
            })?;
        }
        let now = Utc::now();
        let entry = SessionEntry {
            id: id.to_string(),
            display_title: format!("fork of {}", original.label()),
            records_path: target,
            created_at: now,
            last_active_at: now,
            provider: original.provider.clone(),
            model: original.model.clone(),
            cwd: original.cwd.clone(),
        };
        self.upsert(entry.clone()).await?;
        Ok(entry)
    }

    /// Write the index whole: to a file of its own, then renamed into
    /// place, so a reader never sees half of it.
    async fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&self.entries)
            .map_err(|e| format!("failed to serialize session index: {e}"))?;
        let staged = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        tokio::fs::write(&staged, json)
            .await
            .map_err(|e| format!("failed to write session index: {e}"))?;
        tokio::fs::rename(&staged, &self.path)
            .await
            .map_err(|e| format!("failed to write session index: {e}"))?;
        Ok(())
    }
}

/// Insert `entry`, or replace the one with its id.
fn put(entries: &mut Vec<SessionEntry>, entry: SessionEntry) {
    if let Some(existing) = entries.iter_mut().find(|e| e.id == entry.id) {
        *existing = entry;
    } else {
        entries.push(entry);
    }
}

/// The index at `path`; empty when there is none, or it cannot be read.
async fn read_entries(path: &std::path::Path) -> Vec<SessionEntry> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// The index's write lock: `index.json.lock`, made only while one process
/// reads, changes and writes the index. A writer waits a few seconds for
/// another; a lock older than that is a crashed writer's, and is removed.
/// Without the lock — it cannot be made, or another writer keeps it — the
/// write goes ahead, the index merely at risk of what it was before.
struct IndexLock(Option<PathBuf>);

impl IndexLock {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(3);
    const STALE: std::time::Duration = std::time::Duration::from_secs(10);

    async fn acquire(index: &std::path::Path) -> Self {
        let path = index.with_extension("json.lock");
        let deadline = tokio::time::Instant::now() + Self::WAIT;
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Self(Some(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|at| at.elapsed().ok())
                        .is_some_and(|age| age > Self::STALE);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                }
                Err(_) => return Self(None),
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!("the session index is locked by another writer; writing anyway");
                return Self(None);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            display_title: id.to_string(),
            records_path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            provider: "kimi".to_string(),
            model: "kimi-k2-0711-preview".to_string(),
            cwd: None,
        }
    }

    #[tokio::test]
    async fn test_upsert_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().to_path_buf()).await;

        let mut a = entry("a");
        a.last_active_at = Utc::now() - chrono::Duration::days(1);
        let b = entry("b");
        store.upsert(a).await.unwrap();
        store.upsert(b.clone()).await.unwrap();

        let list = store.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "b");
        assert_eq!(list[1].id, "a");
    }

    /// Two processes' stores of one index each keep what the other wrote:
    /// a session started in another terminal stays listed.
    #[tokio::test]
    async fn another_processs_sessions_survive_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let mut ours = SessionStore::new(dir.path().to_path_buf()).await;
        let mut theirs = SessionStore::new(dir.path().to_path_buf()).await;
        theirs.upsert(entry("theirs")).await.unwrap();
        ours.upsert(entry("ours")).await.unwrap();

        let reopened = SessionStore::new(dir.path().to_path_buf()).await;
        let mut ids: Vec<String> = reopened.list().into_iter().map(|e| e.id).collect();
        ids.sort();
        assert_eq!(ids, ["ours", "theirs"]);
        assert!(!dir.path().join("index.json.lock").exists());
    }

    /// A title another process set since this store opened stays.
    #[tokio::test]
    async fn a_title_set_elsewhere_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let mut ours = SessionStore::new(dir.path().to_path_buf()).await;
        ours.upsert(SessionEntry {
            display_title: String::new(),
            ..entry("work")
        })
        .await
        .unwrap();
        let mut theirs = SessionStore::new(dir.path().to_path_buf()).await;
        assert!(
            theirs
                .title_if_untitled("work", "named elsewhere")
                .await
                .unwrap()
        );

        assert!(!ours.title_if_untitled("work", "auto title").await.unwrap());
        assert_eq!(ours.get("work").unwrap().display_title, "named elsewhere");
    }

    /// A lock a crashed writer left behind, old enough, does not hold the
    /// index.
    #[tokio::test]
    async fn a_stale_index_lock_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("index.json.lock");
        let file = std::fs::File::create(&lock).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        let mut store = SessionStore::new(dir.path().to_path_buf()).await;
        let started = std::time::Instant::now();
        store.upsert(entry("x")).await.unwrap();
        assert!(
            started.elapsed() < IndexLock::WAIT,
            "it did not wait the lock out"
        );
        assert!(!lock.exists());
    }

    #[tokio::test]
    async fn test_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let mut store = SessionStore::new(path.clone()).await;
            store.upsert(entry("x")).await.unwrap();
        }
        {
            let store = SessionStore::new(path).await;
            assert!(store.get("x").is_some());
        }
    }

    /// A fork copies the records into a new session beside the original,
    /// which stays as it was.
    #[tokio::test]
    async fn a_fork_copies_the_records_into_a_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().to_path_buf()).await;
        let records = dir.path().join("work.jsonl");
        std::fs::write(&records, "{\"line\":1}\n").unwrap();
        store
            .upsert(SessionEntry {
                records_path: records.clone(),
                cwd: Some(PathBuf::from("/repo")),
                ..entry("work")
            })
            .await
            .unwrap();

        let fork = store.fork("work", "work-2").await.unwrap();
        assert_eq!(fork.records_path, dir.path().join("work-2.jsonl"));
        assert_eq!(
            std::fs::read_to_string(&fork.records_path).unwrap(),
            "{\"line\":1}\n"
        );
        assert_eq!(fork.display_title, "fork of work");
        assert_eq!(fork.cwd.as_deref(), Some(std::path::Path::new("/repo")));
        assert!(store.get("work").is_some());

        assert!(
            store
                .fork("work", "work-2")
                .await
                .unwrap_err()
                .contains("already exists")
        );
        assert!(
            store
                .fork("missing", "x")
                .await
                .unwrap_err()
                .contains("no session")
        );
    }

    /// A fork of a session with cron tasks starts without them and is told
    /// so once, as a `<system-reminder>`; a fork of a session without tasks
    /// is told nothing.
    #[tokio::test]
    async fn a_fork_is_told_once_that_cron_tasks_stayed_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().to_path_buf()).await;
        let reminders = |path: &std::path::Path| {
            std::fs::read_to_string(path)
                .unwrap()
                .matches(agent_core::cron::CRON_FORK_CLEARED_REMINDER)
                .count()
        };
        for (id, tasks) in [("busy", true), ("idle", false)] {
            let records = dir.path().join(format!("{id}.jsonl"));
            std::fs::write(&records, "").unwrap();
            if tasks {
                let cron =
                    agent_core::cron::CronManager::load(&sidecar_path(&records, "cron"), false);
                cron.add("*/5 * * * *", "check the queue", true).unwrap();
            }
            store
                .upsert(SessionEntry {
                    records_path: records,
                    ..entry(id)
                })
                .await
                .unwrap();
        }

        let busy = store.fork("busy", "busy-2").await.unwrap();
        assert_eq!(reminders(&busy.records_path), 1);
        let replayed = agent_core::records::Records::new(&busy.records_path)
            .load()
            .await
            .unwrap();
        assert!(
            replayed[0].text_content().starts_with("<system-reminder>"),
            "{:?}",
            replayed[0]
        );
        assert!(
            !sidecar_path(&busy.records_path, "cron").exists(),
            "the tasks stay with the source"
        );

        let idle = store.fork("idle", "idle-2").await.unwrap();
        assert_eq!(reminders(&idle.records_path), 0);
    }

    /// A fork is resumed like the original, so the session state a resume
    /// loads goes with it; a sidecar the original lacks is not invented.
    #[tokio::test]
    async fn a_fork_takes_the_session_state_along() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().to_path_buf()).await;
        let records = dir.path().join("work.jsonl");
        std::fs::write(&records, "{\"line\":1}\n").unwrap();
        std::fs::write(sidecar_path(&records, "todos"), "[\"todo\"]").unwrap();
        std::fs::write(sidecar_path(&records, "reads"), "{\"reads\":1}").unwrap();
        store
            .upsert(SessionEntry {
                records_path: records.clone(),
                ..entry("work")
            })
            .await
            .unwrap();

        let fork = store.fork("work", "work-2").await.unwrap();
        let copied = |kind: &str| std::fs::read_to_string(sidecar_path(&fork.records_path, kind));
        assert_eq!(copied("todos").unwrap(), "[\"todo\"]");
        assert_eq!(copied("reads").unwrap(), "{\"reads\":1}");
        assert!(copied("goals").is_err(), "no goals sidecar to copy");
        assert_eq!(
            std::fs::read_to_string(sidecar_path(&records, "todos")).unwrap(),
            "[\"todo\"]",
            "the original keeps its own"
        );
    }

    /// A title outlives later activity: a fork keeps "fork of …" once it is
    /// resumed, and a `/title` holds until the next one.
    #[tokio::test]
    async fn titles_survive_later_activity() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = SessionStore::new(dir.path().to_path_buf()).await;
        let activity = |id: &str, title: Option<&str>| Activity {
            id: id.to_string(),
            display_title: title.map(str::to_string),
            records_path: dir.path().join(format!("{id}.jsonl")),
            provider: "glm".to_string(),
            model: "glm-5.3".to_string(),
            cwd: dir.path().to_path_buf(),
        };

        store.record_activity(activity("work", None)).await.unwrap();
        assert_eq!(store.get("work").unwrap().display_title, "");
        assert_eq!(store.get("work").unwrap().label(), "work");
        // A title made for the session takes only while it has none.
        assert!(store.title_if_untitled("work", "Lexer port").await.unwrap());
        assert!(!store.title_if_untitled("work", "Other").await.unwrap());
        assert!(!store.title_if_untitled("nowhere", "Other").await.unwrap());
        assert_eq!(store.get("work").unwrap().label(), "Lexer port");
        let reopened = SessionStore::new(dir.path().to_path_buf()).await;
        assert_eq!(reopened.get("work").unwrap().label(), "Lexer port");

        store
            .record_activity(activity("work", Some("Parser rewrite")))
            .await
            .unwrap();
        store.record_activity(activity("work", None)).await.unwrap();
        assert_eq!(store.get("work").unwrap().label(), "Parser rewrite");

        std::fs::write(dir.path().join("work.jsonl"), "").unwrap();
        store.fork("work", "work-2").await.unwrap();
        store
            .record_activity(activity("work-2", None))
            .await
            .unwrap();
        assert_eq!(
            store.get("work-2").unwrap().label(),
            "fork of Parser rewrite"
        );
    }
}
