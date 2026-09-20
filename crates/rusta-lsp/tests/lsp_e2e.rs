//! Real-`rust-analyzer` e2e tests for the opt-in LSP enrichment — ADR §9.
//!
//! `#[ignore]`d for the same reason the GGUF tests are: they need a real
//! language server on `PATH`, so default runs never depend on it being
//! installed. Run with:
//!
//! ```text
//! cargo test -p rusta-lsp --features lsp --test lsp_e2e -- --ignored --nocapture
//! ```
//!
//! ADR §17.2 lists this as a standing limitation rather than coverage: like
//! the embedded backend, this path has never executed under CI.
#![cfg(feature = "lsp")]

use std::fs;

use rusta_lsp::{CodeIntel, Config};
use rusta_repomap::definition_anchor;
use std::time::Duration;
use tempfile::TempDir;

/// Deadline for these tests, far above [`Config::default`]'s 1.5 s.
///
/// They assert *correctness* — that the scaffold's anchor resolves a real
/// signature — not latency. `rust-analyzer`'s time to first useful answer
/// swings with machine load: measured at ~357 ms on a warm box and over
/// 1.5 s on a cold one, which made this suite flaky against the shipped
/// default. The product default stays 1.5 s on purpose (it bounds what an
/// optional annotation may cost a turn); a test that inherited it would be
/// measuring the machine.
const TEST_DEADLINE: Duration = Duration::from_secs(20);

/// Test configuration: the production defaults, with only the deadline
/// relaxed.
fn test_config() -> Config {
    Config {
        deadline: TEST_DEADLINE,
        ..Config::default()
    }
}

/// A minimal but *real* cargo crate: `rust-analyzer` needs a manifest and a
/// crate root before it will resolve anything.
fn fixture_crate() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::create_dir_all(dir.path().join("src")).expect("src dir");
    fs::write(
        dir.path().join("src/lib.rs"),
        "/// Doubles its argument.\npub fn double(value: u32) -> u32 {\n    value * 2\n}\n",
    )
    .expect("lib.rs");
    dir
}

#[tokio::test]
#[ignore = "requires rust-analyzer on PATH"]
async fn resolves_a_real_signature_through_the_scaffold_anchor() {
    let dir = fixture_crate();
    let root = dir.path().to_path_buf();

    // The scaffold supplies the coordinate — this is the whole integration in
    // one line, and the reason no model ever sees a line or column.
    let anchor = definition_anchor(&root, "src/lib.rs", "double").expect("anchor resolves");

    let intel = CodeIntel::enabled(root, test_config());
    let signature = intel
        .signature("src/lib.rs", anchor.line, anchor.character)
        .await
        .expect("rust-analyzer resolved a signature");

    let text = signature.to_string();
    assert!(
        text.contains("double"),
        "signature should name the function, got: {text}"
    );
    assert!(
        text.contains("u32"),
        "signature should carry resolved types, got: {text}"
    );
    assert!(
        !text.contains("Doubles its argument"),
        "doc prose must be stripped, got: {text}"
    );
    assert_eq!(text.lines().count(), 1, "annotation is exactly one line");

    intel.shutdown().await;
}

#[tokio::test]
#[ignore = "requires rust-analyzer on PATH"]
async fn a_warm_server_reports_a_barren_position_promptly() {
    let dir = fixture_crate();
    let root = dir.path().to_path_buf();
    let intel = CodeIntel::enabled(root.clone(), test_config());

    // Warm the session first. Until a server has resolved something, an empty
    // hover is indistinguishable from an unfinished index, so the client keeps
    // polling — and this assertion would be satisfied by the deadline expiring
    // rather than by the position genuinely having no type.
    let anchor = definition_anchor(&root, "src/lib.rs", "double").expect("anchor");
    intel
        .signature("src/lib.rs", anchor.line, anchor.character)
        .await
        .expect("warm-up hover resolved");

    // Column 1 of the doc-comment line holds no resolvable symbol.
    let started = std::time::Instant::now();
    assert!(
        intel.signature("src/lib.rs", 1, 1).await.is_none(),
        "a barren position degrades to no annotation, not an error"
    );
    assert!(
        started.elapsed() < TEST_DEADLINE,
        "a warm server must answer a barren position without spending the \
         whole deadline; took {:?}",
        started.elapsed()
    );

    intel.shutdown().await;
}

#[tokio::test]
#[ignore = "requires rust-analyzer on PATH"]
async fn a_path_outside_the_workspace_is_refused() {
    let dir = fixture_crate();
    let intel = CodeIntel::enabled(dir.path().to_path_buf(), test_config());

    let started = std::time::Instant::now();
    assert!(
        intel.signature("../escape.rs", 1, 1).await.is_none(),
        "paths outside the workspace root must never reach the server"
    );
    // Regression: a *permanent* error must not be mistaken for a server that
    // is still indexing. It once was, so every drill of a file the server
    // cannot handle spent the entire deadline re-asking a question whose
    // answer could not change — 20s of a 20s budget here, 1.5s of every such
    // drill in production.
    assert!(
        started.elapsed() < TEST_DEADLINE / 2,
        "a rejected path must fail fast, not poll; took {:?}",
        started.elapsed()
    );

    intel.shutdown().await;
}
