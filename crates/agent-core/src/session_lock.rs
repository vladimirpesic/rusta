//! One process writes a session at a time.
//!
//! Two Rusta processes on one session — two terminals on the `default`
//! session, a REPL and a `-p` run on the same `--session` — append to one
//! records file, each unaware of the other's turns. A session is therefore
//! locked while a process writes it: a lock file beside its records file
//! (`default.jsonl` has `default.lock`) names the holder (its pid, host,
//! start time and client). It is held while a turn or a compaction
//! runs and released when it ends, so another process may use the session
//! between them; that one finds the records grown and reloads them before it
//! writes (see [`crate::records::Records::appended_elsewhere`]).
//!
//! A lock whose process has exited — or is a zombie, or started before the
//! last boot, its pid since reused — is taken over silently. One judged alive
//! by mistake can be taken over on request ([`SessionLocks::take_over`]).
//! `RUSTA_SESSION_LOCK=0` turns the lock off. A lock that cannot be written (a
//! read-only data directory) is not fatal: the session runs unlocked, with a
//! warning.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// The variable that turns the lock off with `0` (or `false`, `no`, `off`).
pub const SESSION_LOCK_ENV: &str = "RUSTA_SESSION_LOCK";

const LOCK_FILE_VERSION: u32 = 1;

/// Who holds a session's lock, as its lock file names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockHolder {
    pub version: u32,
    pub token: String,
    pub pid: u32,
    pub hostname: String,
    /// When the holder took the lock, in milliseconds since the epoch.
    pub started_at: i64,
    /// What the holder is: `rusta`, and how it runs (`repl`, `headless`…).
    pub client: String,
}

impl LockHolder {
    /// The holder as a person reads it: `rusta repl (pid 4242 on box)`.
    #[must_use]
    pub fn describe(&self) -> String {
        if self.hostname.is_empty() {
            format!("{} (pid {})", self.client, self.pid)
        } else {
            format!("{} (pid {} on {})", self.client, self.pid, self.hostname)
        }
    }
}

/// A session another process is writing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}", occupied_message(.session, .holder.as_ref()))]
pub struct SessionOccupied {
    pub session: String,
    /// The holder, when its lock file could be read.
    pub holder: Option<LockHolder>,
}

fn occupied_message(session: &str, holder: Option<&LockHolder>) -> String {
    let by = holder.map_or_else(
        || "another Rusta process".to_string(),
        |holder| format!("another Rusta process, {}", holder.describe()),
    );
    format!(
        "Session '{session}' is in use by {by}. Stop the session there or exit it, then try \
         again."
    )
}

/// What a claim of a lock file found.
enum Claim {
    Claimed,
    Held(Option<LockHolder>),
}

/// A session this process holds: the token its lock file carries, and how
/// many leases want it.
struct Held {
    token: String,
    leases: usize,
}

/// A session as its lock knows it: its id, which messages name, and its
/// records file, beside which the lock lives.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LockedSession {
    pub id: String,
    pub records: PathBuf,
}

impl LockedSession {
    #[must_use]
    pub fn new(id: impl Into<String>, records: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            records: records.into(),
        }
    }

    /// The lock file: the records file's, with `.lock` for its extension.
    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.records.with_extension("lock")
    }
}

/// The session locks of this process.
pub struct SessionLocks {
    hostname: String,
    client: String,
    disabled: bool,
    held: Mutex<HashMap<PathBuf, Held>>,
}

impl SessionLocks {
    /// The locks of a process that is `client` (`rusta repl`, `rusta
    /// headless`); off when [`SESSION_LOCK_ENV`] says so.
    #[must_use]
    pub fn new(client: impl Into<String>) -> Self {
        let disabled = std::env::var(SESSION_LOCK_ENV).is_ok_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        });
        Self::with_options(client, disabled)
    }

    fn with_options(client: impl Into<String>, disabled: bool) -> Self {
        Self {
            hostname: system::hostname(),
            client: client.into(),
            disabled,
            held: Mutex::new(HashMap::new()),
        }
    }

    /// A lease on `session`'s lock, taking the lock if this process does not
    /// hold it yet. The lock is released when the last lease ends.
    ///
    /// # Errors
    /// [`SessionOccupied`] when another live process holds it.
    pub fn lease(
        self: &Arc<Self>,
        session: &LockedSession,
    ) -> Result<SessionLease, SessionOccupied> {
        let path = session.lock_path();
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = held.get_mut(&path) {
            entry.leases += 1;
        } else {
            let token = self.publish(session, &path)?;
            held.insert(path.clone(), Held { token, leases: 1 });
        }
        Ok(SessionLease {
            locks: Arc::clone(self),
            path,
        })
    }

    /// Remove `session`'s lock, whoever holds it: for a lock judged alive by
    /// mistake (another host's, a reused pid). The holder it named, `None`
    /// when this process holds the lock, or nobody does.
    ///
    /// # Errors
    /// The lock file could not be removed, for one another user owns.
    pub fn take_over(&self, session: &LockedSession) -> std::io::Result<Option<LockHolder>> {
        let path = session.lock_path();
        let held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if self.disabled || held.contains_key(&path) {
            return Ok(None);
        }
        let holder = read_holder(&path);
        if let Some(holder) = &holder {
            tracing::warn!(
                "taking over session '{}' from {}",
                session.id,
                holder.describe()
            );
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(holder),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Whether this process holds `session`'s lock.
    #[must_use]
    pub fn holds(&self, session: &LockedSession) -> bool {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&session.lock_path())
    }

    /// Release every lock this process holds: at exit.
    pub fn release_all(&self) {
        let held = std::mem::take(&mut *self.held.lock().unwrap_or_else(|e| e.into_inner()));
        for (path, entry) in held {
            self.unpublish(&path, &entry.token);
        }
    }

    fn release(&self, path: &Path) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = held.get_mut(path) else {
            return;
        };
        entry.leases -= 1;
        if entry.leases == 0
            && let Some(entry) = held.remove(path)
        {
            self.unpublish(path, &entry.token);
        }
    }

    /// Write the lock file `path` of `session`, and return its token.
    fn publish(&self, session: &LockedSession, path: &Path) -> Result<String, SessionOccupied> {
        let token = format!("{}:{}", std::process::id(), ulid::Ulid::generate());
        if self.disabled {
            return Ok(token);
        }
        match self.claim(path, &token) {
            Ok(Claim::Claimed) => Ok(token),
            Ok(Claim::Held(holder)) => Err(SessionOccupied {
                session: session.id.clone(),
                holder,
            }),
            Err(e) => {
                tracing::warn!(
                    "the lock of session '{}' is unavailable ({e}); continuing without it",
                    session.id
                );
                Ok(token)
            }
        }
    }

    fn unpublish(&self, path: &Path, token: &str) {
        if self.disabled {
            return;
        }
        if read_holder(path).is_some_and(|holder| holder.token == token) {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Create `path` holding this process; take it over once if its holder
    /// is gone.
    fn claim(&self, path: &Path, token: &str) -> std::io::Result<Claim> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        for _ in 0..2 {
            if self.try_publish(path, token)? {
                return Ok(Claim::Claimed);
            }
            match read_holder(path) {
                Some(holder) if self.alive(&holder) => return Ok(Claim::Held(Some(holder))),
                _ => self.reclaim(path, token)?,
            }
        }
        Ok(Claim::Held(read_holder(path)))
    }

    /// Remove `path`, whose holder is gone, unless another process is
    /// already doing so: a `.reclaim` marker makes one of them do it.
    fn reclaim(&self, path: &Path, token: &str) -> std::io::Result<()> {
        let marker = path.with_extension("lock.reclaim");
        if !self.try_publish(&marker, token)? {
            if read_holder(&marker).is_none_or(|holder| !self.alive(&holder)) {
                let _ = std::fs::remove_file(&marker);
            }
            return Ok(());
        }
        if read_holder(path).is_none_or(|holder| !self.alive(&holder)) {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(&marker);
        Ok(())
    }

    /// Create `path` with this process as its holder, atomically: written
    /// to a file of its own, then linked into place. `false` when it exists.
    fn try_publish(&self, path: &Path, token: &str) -> std::io::Result<bool> {
        let holder = LockHolder {
            version: LOCK_FILE_VERSION,
            token: token.to_string(),
            pid: std::process::id(),
            hostname: self.hostname.clone(),
            started_at: chrono::Utc::now().timestamp_millis(),
            client: self.client.clone(),
        };
        let body = serde_json::to_vec(&holder).map_err(std::io::Error::other)?;
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let staged = path.with_file_name(format!("{name}.{}.tmp", token.replace(':', "-")));
        std::fs::write(&staged, body)?;
        let linked = std::fs::hard_link(&staged, path);
        let _ = std::fs::remove_file(&staged);
        match linked {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Whether `holder` still runs: another host's is taken to; a process
    /// here must exist, not be a zombie, and have taken the lock since boot.
    fn alive(&self, holder: &LockHolder) -> bool {
        if !holder.hostname.is_empty() && holder.hostname != self.hostname {
            return true;
        }
        if !system::process_alive(holder.pid) {
            return false;
        }
        system::boot_time_millis().is_none_or(|boot| holder.started_at >= boot)
    }
}

/// A lease on a session's lock; the lock goes with the last one.
pub struct SessionLease {
    locks: Arc<SessionLocks>,
    path: PathBuf,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        self.locks.release(&self.path);
    }
}

/// The holder `path` names: `None` when there is no file, and an unknown
/// holder (pid 0, which is never alive) when it cannot be read.
fn read_holder(path: &Path) -> Option<LockHolder> {
    let text = match std::fs::read(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(_) => return Some(unknown_holder()),
    };
    Some(serde_json::from_slice(&text).unwrap_or_else(|_| unknown_holder()))
}

fn unknown_holder() -> LockHolder {
    LockHolder {
        version: 0,
        token: String::new(),
        pid: 0,
        hostname: String::new(),
        started_at: 0,
        client: "unknown".to_string(),
    }
}

/// What the lock asks of the system, without `unsafe` code: `/proc` on
/// Linux, `ps`, `sysctl` and `hostname` elsewhere.
mod system {
    use std::process::{Command, Stdio};

    /// Whether process `pid` exists and is not a zombie. A process that
    /// cannot be inspected is taken to be alive.
    pub fn process_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                // The state follows the parenthesised command name.
                Ok(stat) => stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .is_none_or(|state| state != "Z" && state != "X"),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => true,
            }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            match Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "stat="])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
            {
                Ok(out) => {
                    let state = String::from_utf8_lossy(&out.stdout);
                    let state = state.trim();
                    out.status.success() && !state.is_empty() && !state.starts_with('Z')
                }
                Err(_) => true,
            }
        }
        #[cfg(windows)]
        {
            match Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
            {
                Ok(out) => String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\"")),
                Err(_) => true,
            }
        }
    }

    /// When the system booted, in milliseconds since the epoch, where it
    /// can be read.
    pub fn boot_time_millis() -> Option<i64> {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string("/proc/stat").ok()?;
            let seconds: i64 = stat
                .lines()
                .find_map(|line| line.strip_prefix("btime "))?
                .trim()
                .parse()
                .ok()?;
            Some(seconds * 1000)
        }
        #[cfg(target_os = "macos")]
        {
            // `{ sec = 1729500000, usec = 0 } Mon Oct 21 …`
            let out = Command::new("sysctl")
                .args(["-n", "kern.boottime"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .ok()?;
            let text = String::from_utf8_lossy(&out.stdout);
            let seconds: i64 = text
                .split("sec =")
                .nth(1)?
                .split(',')
                .next()?
                .trim()
                .parse()
                .ok()?;
            Some(seconds * 1000)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }

    /// This machine's name; empty when it cannot be read.
    pub fn hostname() -> String {
        #[cfg(target_os = "linux")]
        if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            return name.trim().to_string();
        }
        Command::new("hostname")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locks(client: &str) -> Arc<SessionLocks> {
        Arc::new(SessionLocks::with_options(client, false))
    }

    fn session(dir: &Path, id: &str) -> LockedSession {
        LockedSession::new(id, dir.join(format!("{id}.jsonl")))
    }

    /// The lock file of `session`, as a holder `pid` started at
    /// `started_at` on `hostname` wrote it.
    fn plant(session: &LockedSession, pid: u32, hostname: &str, started_at: i64) {
        let holder = LockHolder {
            version: 1,
            token: "planted".into(),
            pid,
            hostname: hostname.into(),
            started_at,
            client: "rusta repl".into(),
        };
        std::fs::write(session.lock_path(), serde_json::to_vec(&holder).unwrap()).unwrap();
    }

    /// A lease takes the lock, beside the records file; a second one shares
    /// it; it goes with the last.
    #[test]
    fn the_lock_goes_with_the_last_lease() {
        let dir = tempfile::tempdir().unwrap();
        let ours = locks("rusta repl");
        let default = session(dir.path(), "default");
        assert_eq!(default.lock_path(), dir.path().join("default.lock"));

        let first = ours.lease(&default).unwrap();
        let holder = read_holder(&default.lock_path()).unwrap();
        assert_eq!(holder.pid, std::process::id());
        assert_eq!(holder.client, "rusta repl");
        let second = ours.lease(&default).unwrap();
        drop(first);
        assert!(default.lock_path().exists() && ours.holds(&default));
        drop(second);
        assert!(!default.lock_path().exists() && !ours.holds(&default));
    }

    /// Another live process's lock refuses the session, naming the holder.
    #[cfg(unix)]
    #[test]
    fn a_live_holder_refuses_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let default = session(dir.path(), "default");
        plant(
            &default,
            other.id(),
            &system::hostname(),
            chrono::Utc::now().timestamp_millis(),
        );
        let ours = locks("rusta headless");

        let Err(occupied) = ours.lease(&default) else {
            panic!("a live holder keeps the session");
        };
        assert_eq!(occupied.holder.clone().unwrap().pid, other.id());
        let message = occupied.to_string();
        assert!(
            message.contains(&format!(
                "Session 'default' is in use by another Rusta process, rusta repl (pid {}",
                other.id()
            )),
            "{message}"
        );
        assert!(message.contains("Stop the session there"), "{message}");

        other.kill().unwrap();
        other.wait().unwrap();
        assert!(ours.lease(&default).is_ok(), "an exited holder lets go");
    }

    /// A lock left by a process that is gone, or that predates the last boot
    /// (its pid since reused), or that cannot be read, is taken over.
    #[test]
    fn a_dead_pre_boot_or_garbled_holder_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let hostname = system::hostname();
        let ours = locks("rusta repl");

        let gone = session(dir.path(), "gone");
        plant(&gone, u32::MAX - 1, &hostname, 0);
        assert!(ours.lease(&gone).is_ok());

        if let Some(boot) = system::boot_time_millis() {
            // This very process, as if it held the lock before the boot.
            let reused = session(dir.path(), "reused");
            plant(&reused, std::process::id(), &hostname, boot - 1);
            assert!(locks("rusta headless").lease(&reused).is_ok());
        }

        let garbled = session(dir.path(), "garbled");
        std::fs::write(garbled.lock_path(), b"{not json").unwrap();
        assert!(ours.lease(&garbled).is_ok());
    }

    /// Another host's lock is taken to be alive; taking over removes it.
    #[test]
    fn another_hosts_lock_is_taken_over_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let shared = session(dir.path(), "shared");
        plant(&shared, 1, "elsewhere.example", 0);
        let ours = locks("rusta repl");
        let occupied = ours
            .lease(&shared)
            .err()
            .expect("another host's lock holds");
        assert_eq!(occupied.holder.unwrap().hostname, "elsewhere.example");

        let taken = ours.take_over(&shared).unwrap().unwrap();
        assert_eq!(taken.hostname, "elsewhere.example");
        let lease = ours.lease(&shared).unwrap();
        // Taking over a session this process holds leaves it alone.
        assert_eq!(ours.take_over(&shared).unwrap(), None);
        assert!(ours.holds(&shared));
        drop(lease);
    }

    /// Two records files of one session id are two sessions to the lock.
    #[test]
    fn the_records_file_is_what_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let ours = locks("rusta repl");
        let here = ours.lease(&session(dir.path(), "default")).unwrap();
        let theirs = locks("rusta headless");
        assert!(theirs.lease(&session(elsewhere.path(), "default")).is_ok());
        drop(here);
    }

    /// A lease ending removes only this process's own lock file.
    #[test]
    fn a_lease_removes_only_its_own_lock() {
        let dir = tempfile::tempdir().unwrap();
        let ours = locks("rusta repl");
        let default = session(dir.path(), "default");
        let lease = ours.lease(&default).unwrap();
        // Taken over by someone else meanwhile.
        plant(&default, 1, "elsewhere.example", 0);
        drop(lease);
        assert_eq!(
            read_holder(&default.lock_path()).unwrap().hostname,
            "elsewhere.example"
        );
    }

    /// Turned off, the lock writes nothing and refuses nothing.
    #[test]
    fn a_disabled_lock_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let shared = session(dir.path(), "shared");
        plant(&shared, 1, "elsewhere.example", 0);
        let off = Arc::new(SessionLocks::with_options("rusta", true));
        let lease = off.lease(&shared).unwrap();
        let default = session(dir.path(), "default");
        let _other = off.lease(&default).unwrap();
        assert!(!default.lock_path().exists());
        drop(lease);
    }

    /// The release at exit lets every held session go.
    #[test]
    fn release_all_lets_every_session_go() {
        let dir = tempfile::tempdir().unwrap();
        let ours = locks("rusta repl");
        let (a, b) = (session(dir.path(), "a"), session(dir.path(), "b"));
        let leases = (ours.lease(&a).unwrap(), ours.lease(&b).unwrap());
        ours.release_all();
        assert!(!ours.holds(&a) && !ours.holds(&b));
        assert!(!a.lock_path().exists() && !b.lock_path().exists());
        drop(leases);
    }

    /// This process is alive and not a zombie; pid 0 never is.
    #[test]
    fn this_process_is_alive() {
        assert!(system::process_alive(std::process::id()));
        assert!(!system::process_alive(0));
    }
}
