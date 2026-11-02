//! Session-level wiring for user hooks: building the session's set from the
//! config, SessionStart/SessionEnd, the heartbeat task, and the Ctrl+C that
//! forces the exit while shutdown hooks run.

use std::{future::Future, path::Path, sync::Arc, time::Duration};

use agent_core::user_hooks::{UserHookEvent, UserHookRule, UserHooks};

/// Build the session's hook set from the user config's rules. `None` when no
/// rules are configured. `session_id` is the session or run id, `cwd` the
/// project directory the hooks run in.
pub fn build(
    rules: Vec<UserHookRule>,
    session_id: &str,
    session_title: &str,
    cwd: &Path,
) -> anyhow::Result<Option<Arc<UserHooks>>> {
    if rules.is_empty() {
        return Ok(None);
    }
    let hooks = UserHooks::new(
        rules,
        session_id,
        session_title,
        "rusta_cli",
        cwd.to_path_buf(),
    )
    .map_err(|e| anyhow::anyhow!("invalid [[hooks]] configuration: {e}"))?;
    Ok(Some(Arc::new(hooks)))
}

/// Fire SessionStart after the session starts or resumes. `source` is
/// `startup` or `resume`, the matcher's input.
pub async fn session_start(hooks: &UserHooks, source: &str, model: &str, profile: Option<&str>) {
    let mut fields = serde_json::Map::new();
    fields.insert(
        "source".to_string(),
        serde_json::Value::String(source.to_string()),
    );
    fields.insert(
        "model".to_string(),
        serde_json::Value::String(model.to_string()),
    );
    fields.insert(
        "profile".to_string(),
        serde_json::Value::String(profile.unwrap_or("").to_string()),
    );
    hooks
        .fire(UserHookEvent::SessionStart, source, fields)
        .await;
}

/// How [`session_end`] finished.
pub enum SessionEnd {
    /// The hooks ran to completion (or none listen for SessionEnd).
    Finished,
    /// Ctrl+C arrived while the shutdown hooks ran; the caller must exit now
    /// without waiting for them.
    Interrupted,
}

/// Run the SessionEnd hooks, showing progress on stderr while they run, and
/// let `interrupt` (a second Ctrl+C) cut the wait short: shutdown hooks must
/// never hold the terminal hostage. Without a SessionEnd hook it says nothing.
pub async fn session_end(
    hooks: Option<&Arc<UserHooks>>,
    interrupt: impl Future<Output = ()>,
) -> SessionEnd {
    let Some(hooks) = hooks.filter(|hooks| hooks.has_event(UserHookEvent::SessionEnd)) else {
        return SessionEnd::Finished;
    };
    let fire = async {
        eprintln!("Running SessionEnd hooks…");
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let outcome = hooks
            .fire_with_progress(
                UserHookEvent::SessionEnd,
                "exit",
                serde_json::Map::from_iter([(
                    "source".to_string(),
                    serde_json::Value::String("exit".to_string()),
                )]),
                |command| {
                    let n = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    eprintln!("  SessionEnd hook {n}: {command}");
                },
            )
            .await;
        if outcome.blocked.is_some() {
            // SessionEnd cannot block; the runner only reports the attempt.
            eprintln!("  (a SessionEnd hook tried to block; ignored)");
        }
        eprintln!("SessionEnd hooks finished.");
        SessionEnd::Finished
    };
    tokio::pin!(fire);
    tokio::select! {
        result = &mut fire => result,
        () = interrupt => {
            eprintln!("Ctrl+C: exiting without waiting for the SessionEnd hooks.");
            SessionEnd::Interrupted
        }
    }
}

/// The next Ctrl+C as a future. Registered fresh: the run's main listener
/// consumed the first signal, so this one only fires on a second press.
pub async fn ctrl_c_again() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::SignalKind;
        if let Ok(mut signal) = tokio::signal::unix::signal(SignalKind::interrupt()) {
            signal.recv().await;
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Spawn the SessionHeartbeat loop: one firing every 60 s with the session's
/// uptime, alive until the process ends. The caller spawns it only when a
/// SessionHeartbeat rule exists, so the timer costs nothing otherwise.
pub fn spawn_heartbeat(hooks: Arc<UserHooks>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await; // the first tick is immediate
        loop {
            interval.tick().await;
            let mut fields = serde_json::Map::new();
            fields.insert(
                "uptime_ms".to_string(),
                serde_json::Value::from(hooks.uptime() as u64),
            );
            hooks
                .fire(UserHookEvent::SessionHeartbeat, "", fields)
                .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn end_hooks(command: &str, timeout: Option<u64>) -> Arc<UserHooks> {
        Arc::new(
            UserHooks::new(
                vec![UserHookRule {
                    event: UserHookEvent::SessionEnd,
                    matcher: None,
                    command: command.to_string(),
                    timeout,
                }],
                "session_test",
                "Test",
                "rusta_cli",
                std::env::temp_dir(),
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn no_hooks_finish_at_once() {
        let result = session_end(None, std::future::pending::<()>()).await;
        assert!(matches!(result, SessionEnd::Finished));
    }

    #[tokio::test]
    async fn finished_hooks_report_finished() {
        let hooks = end_hooks("true", Some(5));
        let result = session_end(Some(&hooks), std::future::pending::<()>()).await;
        assert!(matches!(result, SessionEnd::Finished));
    }

    /// Ctrl+C while shutdown hooks run forces the exit: the wait ends before
    /// the hook could, and reports Interrupted.
    #[tokio::test]
    async fn ctrl_c_during_session_end_forces_the_exit() {
        let hooks = end_hooks("sleep 30", Some(60));
        let start = Instant::now();
        let result = session_end(Some(&hooks), std::future::ready(())).await;
        assert!(
            matches!(result, SessionEnd::Interrupted),
            "Ctrl+C must cut the SessionEnd wait short"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the exit must not wait for the 30 s hook"
        );
    }
}
