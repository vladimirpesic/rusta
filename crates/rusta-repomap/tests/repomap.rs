//! M4 acceptance (DEVELOPMENT_PLAN.md §8): golden snapshot maps on 3 sample
//! repos; token-fitting never exceeds budget; cache hit path tested with
//! mtime bumps; plus `map_drill` span/window behavior. Sample repos are
//! built in tempdirs (no git → the ignore-aware walk path) and snapshots
//! contain only repo-relative paths, so they are deterministic.

use rusta_repomap::{DrillRequest, RepoMap, drill, estimate_tokens};
use std::fs;
use tempfile::TempDir;

/// The three sample repos: rust-heavy, python, mixed go/c (with a def-only
/// C file exercising the word-scan ref backfill inside the map path).
fn sample_repo(kind: &str) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    let write = |rel: &str, content: &str| {
        let path = dir.path().join(rel);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, content).expect("write");
    };
    match kind {
        "rust" => {
            write(
                "src/a.rs",
                "pub fn calculate_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\n\npub struct InvoiceRecord {\n    pub id: u32,\n}\n",
            );
            write(
                "src/b.rs",
                "use crate::a::calculate_total;\nuse crate::a::InvoiceRecord;\n\npub fn render_invoice(rec: &InvoiceRecord) -> String {\n    format!(\"{} {}\", rec.id, calculate_total(&[rec.id]))\n}\n",
            );
            write(
                "src/main.rs",
                "mod a;\nmod b;\n\nfn main() {\n    let rec = a::InvoiceRecord { id: 7 };\n    let text = b::render_invoice(&rec);\n    let total = a::calculate_total(&[1, 2]);\n    println!(\"{text} {total}\");\n}\n",
            );
        }
        "python" => {
            write(
                "store.py",
                "class InventoryManager:\n    def __init__(self):\n        self.stock = {}\n\n    def restock(self, sku, count):\n        self.stock[sku] = count\n",
            );
            write(
                "api.py",
                "from store import InventoryManager\n\ndef handle_restock(payload):\n    mgr = InventoryManager()\n    mgr.restock(payload[\"sku\"], payload[\"count\"])\n    return mgr.stock\n",
            );
        }
        _ => {
            write(
                "main.go",
                "package main\n\nimport \"fmt\"\n\nfunc main() {\n    total := calculate_total([]int{1, 2})\n    fmt.Println(describe(total))\n}\n",
            );
            write(
                "util.go",
                "package main\n\nfunc calculate_total(items []int) int {\n    sum := 0\n    return sum\n}\n\nfunc describe(n int) string {\n    return \"total\"\n}\n",
            );
            write(
                "driver.c",
                "int compute(void) {\n    return helper_value(41) + 1;\n}\n",
            );
        }
    }
    dir
}

fn render(root: &TempDir, budget: usize) -> String {
    RepoMap::new(root.path())
        .with_budget(budget)
        .render_map(&[], None, &[], &[])
}

#[test]
fn golden_maps_on_three_sample_repos() {
    for kind in ["rust", "python", "mixed"] {
        let repo = sample_repo(kind);
        let map = render(&repo, 1024);
        insta::assert_snapshot!(format!("{kind}_repo"), map);
    }
}

#[test]
fn token_fitting_never_exceeds_budget() {
    for kind in ["rust", "python", "mixed"] {
        let repo = sample_repo(kind);
        for budget in [50usize, 200, 1024] {
            let map = render(&repo, budget);
            assert!(
                estimate_tokens(&map) <= budget,
                "{kind} map exceeds {budget}-token budget"
            );
        }
    }
}

#[test]
fn chat_files_steer_ranking_but_never_render() {
    let repo = sample_repo("rust");
    let chat = vec!["src/main.rs".to_string()];
    let with_chat = RepoMap::new(repo.path())
        .with_budget(1024)
        .render_map(&chat, None, &[], &[]);
    insta::assert_snapshot!("chat_repo", with_chat);
    assert!(!with_chat.contains("src/main.rs:"), "chat file rendered");
}

#[test]
fn cache_hit_serves_identical_map_and_mtime_bump_invalidates() {
    let repo = sample_repo("rust");
    let first = render(&repo, 1024);
    // Same RepoMap instance: second render goes through the tag cache.
    let mut map = RepoMap::new(repo.path()).with_budget(1024);
    let a = map.render_map(&[], None, &[], &[]);
    let b = map.render_map(&[], None, &[], &[]);
    assert_eq!(a, b, "cache hit must serve the identical map");
    assert_eq!(a, first, "fresh and cached renders agree");

    // Rewrite + explicit mtime bump (same shape, new identifier).
    fs::write(
        repo.path().join("src/a.rs"),
        "pub fn renamed_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\n\npub struct InvoiceRecord {\n    pub id: u32,\n}\n",
    )
    .expect("rewrite");
    bump_mtime(repo.path().join("src/a.rs"));
    let updated = map.render_map(&[], None, &[], &[]);
    assert_ne!(updated, a, "mtime bump must invalidate the cached tags");
    assert!(updated.contains("renamed_total"), "new def visible");
    // The stale def line must be gone; main.rs may still show its reference.
    assert!(
        !updated.contains("pub fn calculate_total"),
        "stale def line dropped"
    );
}

fn bump_mtime(path: std::path::PathBuf) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600))
        .expect("set mtime");
}

#[test]
fn map_drill_returns_padded_spans_exact_windows_and_errors() {
    let repo = sample_repo("rust");
    let root = repo.path();

    // Span 4-6 of a 6-line file, ±8 clamped at both edges → the whole file:
    // the `use` anchors above the def ride along.
    let def = drill(
        root,
        DrillRequest::Definition {
            path: "src/b.rs",
            name: "render_invoice",
        },
    )
    .expect("drill def");
    assert!(def.starts_with("src/b.rs:1-6"), "padded span header: {def}");
    assert!(def.contains("pub fn render_invoice"));
    assert!(def.contains("use crate::a::calculate_total;"));

    // Span 1-3 of a 7-line file: bottom-clamped to EOF, boundary lines after
    // the def (the struct below) included.
    let top = drill(
        root,
        DrillRequest::Definition {
            path: "src/a.rs",
            name: "calculate_total",
        },
    )
    .expect("drill top def");
    assert!(top.starts_with("src/a.rs:1-7"), "bottom clamp: {top}");
    assert!(top.contains("pub struct InvoiceRecord {"));

    // Model-chosen windows are exact — never padded.
    let window = drill(
        root,
        DrillRequest::Window {
            path: "src/a.rs",
            from: 1,
            to: 2,
        },
    )
    .expect("drill window");
    assert!(window.starts_with("src/a.rs:1-2"));
    assert!(window.contains("calculate_total"));
    assert_eq!(window.lines().count(), 3, "header + exactly 2 lines");

    assert_eq!(
        drill(
            root,
            DrillRequest::Definition {
                path: "src/a.rs",
                name: "missing_fn",
            }
        )
        .unwrap_err(),
        rusta_repomap::DrillError::NotFound {
            path: "src/a.rs".into(),
            name: "missing_fn".into(),
        }
    );
    assert!(matches!(
        drill(
            root,
            DrillRequest::Window {
                path: "src/a.rs",
                from: 2,
                to: 1,
            }
        )
        .unwrap_err(),
        rusta_repomap::DrillError::BadWindow { .. }
    ));
}

#[test]
fn rendering_is_deterministic_across_instances() {
    let repo = sample_repo("mixed");
    let one = render(&repo, 512);
    let two = render(&repo, 512);
    assert_eq!(one, two, "identical inputs must render identical maps");
}
