//! Contract of the opt-in LSP type enrichment on `map_drill` — ADR §14.1.
//!
//! The load-bearing property is a *negative* one: with no language server
//! available, `map_drill` must produce output byte-identical to a build
//! compiled without the feature at all. Everything else about the feature is
//! optional; this is not, because it is what lets the feature ship disabled
//! by default without changing the behaviour the corpus and the golden files
//! describe.
//!
//! Both sides of the feature matrix are pinned here, per ADR §16.6: these
//! tests compile and run in the default graph *and* under `--features lsp`,
//! and must agree.

use std::sync::Arc;

use rusta_llm::{Backend, HttpConfig};
use rusta_tools::{ShellPolicy, Tools};
use serde_json::json;

/// Marker the enrichment prefixes its line with. Duplicated from `map.rs`
/// deliberately: a test that imported the constant would still pass if the
/// constant changed, and this is the string the model actually sees.
const TYPE_MARKER: &str = "⟪type⟫";

const SOURCE: &str = "// a header comment\npub fn alpha(value: u32) -> u32 {\n    value + 1\n}\n\npub fn beta() -> u32 {\n    alpha(1)\n}\n";

fn fixture() -> (tempfile::TempDir, Tools) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("a.rs"), SOURCE).expect("write");
    let backend = Arc::new(Backend::http(HttpConfig::default()).expect("offline construct"));
    let tools = Tools::new(
        dir.path(),
        backend,
        ShellPolicy::standard().expect("policy"),
    )
    .expect("tools");
    (dir, tools)
}

/// T1 + T2 (invariants I5 and I6). `Tools` defaults to a disabled handle, so
/// this is the same assertion in both feature graphs: "feature absent" and
/// "feature present but not enabled" are one observable behaviour.
#[tokio::test]
async fn a_definition_drill_without_a_language_server_is_unannotated() {
    let (_dir, tools) = fixture();
    assert!(
        !tools.code_intel().is_enabled(),
        "Tools must default to disabled enrichment"
    );

    let drilled = tools
        .exec(
            rusta_core::State::Exploring,
            "map_drill",
            &json!({"path": "a.rs", "name": "alpha"}),
        )
        .await;

    assert_eq!(drilled.status, rusta_core::Status::Ok);
    assert!(
        !drilled.content.contains(TYPE_MARKER),
        "no annotation may appear without a language server; got:\n{}",
        drilled.content
    );
    assert!(
        drilled.content.contains("pub fn alpha"),
        "the drill itself must be unaffected; got:\n{}",
        drilled.content
    );
}

/// T3. A `from`/`to` window names no definition, so there is nothing to
/// resolve and the enrichment path is not entered. Observable form of the
/// rule; the live version with a real server is
/// `a_window_drill_is_never_annotated_by_a_real_server` below.
#[tokio::test]
async fn a_window_drill_is_never_annotated() {
    let (_dir, tools) = fixture();

    let drilled = tools
        .exec(
            rusta_core::State::Exploring,
            "map_drill",
            &json!({"path": "a.rs", "from": 2, "to": 4}),
        )
        .await;

    assert!(
        !drilled.content.contains(TYPE_MARKER),
        "a line window must never be annotated; got:\n{}",
        drilled.content
    );
}

/// T6. The read-before-edit ledger credit (§6.3) is what makes a drill count
/// as having seen a region. Enrichment must not disturb it — a drill that
/// stopped crediting would silently break the edit gate.
#[tokio::test]
async fn enrichment_does_not_disturb_the_ledger_credit() {
    let (_dir, tools) = fixture();

    let drilled = tools
        .exec(
            rusta_core::State::Exploring,
            "map_drill",
            &json!({"path": "a.rs", "name": "alpha"}),
        )
        .await;

    assert_eq!(drilled.status, rusta_core::Status::Ok);
    assert!(
        tools
            .editor()
            .ledger()
            .has_read(std::path::Path::new("a.rs")),
        "a successful drill must still credit the ledger"
    );
}

/// T5. The header `cap_window` writes describes the *source* lines returned.
/// The annotation is appended after that rewrite precisely so it cannot
/// inflate the range; this pins the header against a drill whose content is
/// the enrichment's neighbour.
#[tokio::test]
async fn the_drill_header_counts_only_source_lines() {
    let (_dir, tools) = fixture();

    let drilled = tools
        .exec(
            rusta_core::State::Exploring,
            "map_drill",
            &json!({"path": "a.rs", "from": 2, "to": 3}),
        )
        .await;

    let header = drilled.content.lines().next().expect("header");
    assert_eq!(header, "a.rs:2-3", "got:\n{}", drilled.content);
    let body_lines = drilled.content.lines().count() - 1;
    assert_eq!(body_lines, 2, "got:\n{}", drilled.content);
}

/// T4. Sub-coders (§6.8) are handed a disabled handle, so a dispatched
/// read-only actor never spawns or contends for a language server.
#[tokio::test]
async fn sub_coders_are_never_enriched() {
    let (_dir, tools) = fixture();
    // `dispatch` builds its own `ReadOnly` runner internally; what is pinned
    // here is that the registry it is built from carries no enabled handle,
    // which is the precondition the runner relies on.
    assert!(
        !tools.code_intel().is_enabled(),
        "sub-coder drills must not be able to reach a language server"
    );
}

/// The live half of T3, and the only place the enrichment actually runs.
/// `#[ignore]`d like the GGUF tests: it needs `rust-analyzer` on PATH.
///
/// ```text
/// cargo test -p rusta-tools --features lsp --test lsp_enrichment -- --ignored --nocapture
/// ```
#[cfg(feature = "lsp")]
mod live {
    use super::{SOURCE, TYPE_MARKER};
    use std::sync::Arc;

    use rusta_llm::{Backend, HttpConfig};
    use rusta_lsp::{CodeIntel, Config as LspConfig};
    use rusta_tools::{ShellPolicy, Tools};
    use serde_json::json;

    /// A real cargo crate, which `rust-analyzer` needs before it resolves
    /// anything.
    fn live_fixture() -> (tempfile::TempDir, Tools) {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        std::fs::write(dir.path().join("src/lib.rs"), SOURCE).expect("lib.rs");

        let backend = Arc::new(Backend::http(HttpConfig::default()).expect("offline construct"));
        let tools = Tools::new(
            dir.path(),
            backend,
            ShellPolicy::standard().expect("policy"),
        )
        .expect("tools")
        .with_code_intel(CodeIntel::enabled(
            dir.path().to_path_buf(),
            LspConfig {
                // Generous on purpose: these assert correctness, not latency.
                // Time to first useful answer from a cold `rust-analyzer`
                // swings with machine load — ~357 ms warm, over 1.5 s cold —
                // so inheriting the 1.5 s product default would measure the
                // machine rather than the code.
                deadline: std::time::Duration::from_secs(20),
                ..LspConfig::default()
            },
        ));
        (dir, tools)
    }

    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH"]
    async fn a_definition_drill_carries_one_annotation_line() {
        let (_dir, tools) = live_fixture();

        let drilled = tools
            .exec(
                rusta_core::State::Exploring,
                "map_drill",
                &json!({"path": "src/lib.rs", "name": "alpha"}),
            )
            .await;

        let annotations: Vec<&str> = drilled
            .content
            .lines()
            .filter(|line| line.starts_with(TYPE_MARKER))
            .collect();
        assert_eq!(
            annotations.len(),
            1,
            "exactly one annotation line; got:\n{}",
            drilled.content
        );
        assert!(
            annotations[0].contains("alpha"),
            "annotation should name the drilled definition; got: {}",
            annotations[0]
        );
        assert_eq!(
            drilled.content.lines().last(),
            Some(annotations[0]),
            "the annotation is appended last, after the capped source body"
        );

        tools.code_intel().shutdown().await;
    }

    /// The case a four-line fixture crate cannot represent: a real workspace,
    /// where `rust-analyzer` must load an actual crate graph before it can
    /// answer anything.
    ///
    /// This is the test that caught the feature being non-functional in
    /// practice. Against Rusta's own tree, every drill returned nothing while
    /// three separate causes stacked up: a request timeout misclassified as
    /// permanent, a per-request budget larger than the whole deadline, and a
    /// server that only started indexing when the first drill asked for it.
    /// A fixture crate indexes fast enough to hide all three.
    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH; indexes this whole workspace"]
    async fn real_workspace_definitions_resolve_once_indexed() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root is two levels above this crate")
            .to_path_buf();

        let backend = Arc::new(Backend::http(HttpConfig::default()).expect("offline construct"));
        let tools = Tools::new(
            root.clone(),
            backend,
            ShellPolicy::standard().expect("policy"),
        )
        .expect("tools")
        .with_code_intel(CodeIntel::enabled(
            root.clone(),
            LspConfig {
                deadline: std::time::Duration::from_secs(20),
                ..LspConfig::default()
            },
        ));

        // Warm-up began when `enabled` was called. A real session spends this
        // gap on model turns; here it is explicit so the test states what it
        // depends on instead of racing it.
        let drilled = tools
            .exec(
                rusta_core::State::Exploring,
                "map_drill",
                &json!({"path": "crates/rusta-repomap/src/drill.rs", "name": "definition_anchor"}),
            )
            .await;

        let annotation = drilled
            .content
            .lines()
            .find(|line| line.starts_with(TYPE_MARKER))
            .unwrap_or_else(|| {
                panic!(
                    "no type annotation on a real workspace drill; got:\n{}",
                    drilled.content
                )
            });
        assert!(
            annotation.contains("definition_anchor") && annotation.contains("Anchor"),
            "annotation should carry the resolved signature; got: {annotation}"
        );

        tools.code_intel().shutdown().await;
    }

    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH"]
    async fn a_window_drill_is_never_annotated_by_a_real_server() {
        let (_dir, tools) = live_fixture();

        let drilled = tools
            .exec(
                rusta_core::State::Exploring,
                "map_drill",
                &json!({"path": "src/lib.rs", "from": 2, "to": 4}),
            )
            .await;

        assert!(
            !drilled.content.contains(TYPE_MARKER),
            "a line window names no definition, so the server must never be \
             consulted; got:\n{}",
            drilled.content
        );

        tools.code_intel().shutdown().await;
    }
}
