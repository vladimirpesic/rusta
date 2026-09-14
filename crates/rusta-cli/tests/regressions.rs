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

// ---------------------------------------------------- /add selection dialect

/// Shared captured output — what the user would have seen.
#[derive(Clone, Default)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture").extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().expect("capture")).into_owned()
    }
}

/// `/add` keeps shell / Aider path semantics: `*.rs` selects at the repo
/// root, `**/*.rs` recurses, and a bare directory expands to its subtree
/// (Aider's `expand_subdir`). Normalizing slash-free patterns to `**/` here
/// — as the model-facing `glob` tool does — would silently pull in whole
/// trees from a pattern the user typed expecting shell behaviour.
///
/// The one thing that must never happen is the old silent dead end: a
/// slash-free pattern that selects nothing names the recursive form and how
/// many files it would select.
#[tokio::test]
async fn add_uses_path_semantics_and_offers_the_recursive_form() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src/deep")).expect("dirs");
    std::fs::write(dir.path().join("root.rs"), "// root\n").expect("write");
    std::fs::write(dir.path().join("src/lib.rs"), "// lib\n").expect("write");
    std::fs::write(dir.path().join("src/deep/mod.rs"), "// deep\n").expect("write");

    let capture = Capture::default();
    let mut app = App::new(
        Config::default(),
        &Overrides::default(),
        dir.path().to_path_buf(),
        dir.path().join("s.jsonl"),
        Reporter::new(Box::new(capture.clone())),
        Mode::Repl,
    )
    .expect("app");

    // Root-level only, as in a shell — and the dead end now teaches the fix.
    app.handle_line("/add *.rs").await;
    let shown = capture.text();
    assert!(shown.contains("added 1 file"), "{shown}");
    assert!(shown.contains("root.rs"), "{shown}");
    assert!(
        !shown.contains("src/lib.rs"),
        "*.rs must not recurse: {shown}"
    );

    // A pattern that selects nothing at the root names the recursive form.
    app.handle_line("/add *.toml").await;
    let shown = capture.text();
    assert!(shown.contains("no files match *.toml"), "{shown}");

    // The recursive form works and is what the hint points at.
    app.handle_line("/add **/*.rs").await;
    assert!(
        capture.text().contains("added 3 file"),
        "{}",
        capture.text()
    );

    // A bare directory expands to its whole subtree.
    app.handle_line("/drop **/*.rs").await;
    app.handle_line("/add src").await;
    let shown = capture.text();
    assert!(
        shown.contains("added 2 file"),
        "directory expansion: {shown}"
    );
    assert!(shown.contains("src/deep/mod.rs"), "{shown}");
}

/// The hint only fires when it would actually help: it names a recursive
/// form that selects something, and never fires for a plain path typo.
#[tokio::test]
async fn the_recursive_hint_is_never_a_second_dead_end() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("dirs");
    std::fs::write(dir.path().join("src/lib.rs"), "// lib\n").expect("write");

    let capture = Capture::default();
    let mut app = App::new(
        Config::default(),
        &Overrides::default(),
        dir.path().to_path_buf(),
        dir.path().join("s.jsonl"),
        Reporter::new(Box::new(capture.clone())),
        Mode::Repl,
    )
    .expect("app");

    app.handle_line("/add *.rs").await;
    let shown = capture.text();
    assert!(shown.contains("did you mean /add **/*.rs?"), "{shown}");
    assert!(shown.contains("(1 file(s))"), "{shown}");

    // No match anywhere: no hint, because the recursive form is empty too.
    app.handle_line("/add *.zzz").await;
    let tail = capture.text();
    let tail = tail
        .rsplit("no files match *.zzz")
        .next()
        .unwrap_or_default();
    assert!(!tail.contains("did you mean"), "{tail}");

    // A plain path (no metacharacters) never gets a glob hint.
    app.handle_line("/add nope.rs").await;
    let tail = capture.text();
    let tail = tail
        .rsplit("no files match nope.rs")
        .next()
        .unwrap_or_default();
    assert!(!tail.contains("did you mean"), "{tail}");
}
