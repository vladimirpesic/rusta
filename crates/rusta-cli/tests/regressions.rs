//! Regressions for defects found in the 2026-09-13 audit.

use rusta_cli::config::Overrides;
use rusta_cli::render::Reporter;
use rusta_cli::{App, Config, Mode};

/// C4c: a target repo with an unrelated `skills/` directory used to abort
/// startup with a skill-card parse error.
#[test]
fn a_foreign_skills_directory_does_not_block_startup() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("skills")).expect("dirs");
    std::fs::write(
        dir.path().join("skills/onboarding.md"),
        "# Onboarding\n\nWelcome to the project.\n",
    )
    .expect("write");

    let app = App::new(
        Config::default(),
        &Overrides::default(),
        dir.path().to_path_buf(),
        dir.path().join("s.jsonl"),
        Reporter::new(Box::new(Vec::new())),
        Mode::Repl,
    )
    .expect("startup must not depend on the repo's skills/ directory");

    // And the shipped deck is present even though this repo has no cards.
    assert_eq!(app.deck.cards().len(), 4);
}

/// M4: the session log and its diffs sidecar carry whole file contents, so
/// they must not be created group- or world-readable.
#[cfg(unix)]
#[test]
fn session_logs_are_owner_only() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::TempDir::new().expect("tempdir");
    let session = dir.path().join("s.jsonl");
    let _app = App::new(
        Config::default(),
        &Overrides::default(),
        dir.path().to_path_buf(),
        session.clone(),
        Reporter::new(Box::new(Vec::new())),
        Mode::Repl,
    )
    .expect("app");

    let mode = std::fs::metadata(&session)
        .expect("session file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "session log mode was {mode:o}");
}
