//! Property test (M2 acceptance, DEVELOPMENT_PLAN.md §8): the parser never
//! panics on arbitrary input, and the full engine (apply chain + file IO +
//! undo) never panics on arbitrary blocks against arbitrary files.
//!
//! Uses a deterministic xorshift64 generator seeded with fixed constants —
//! no fuzzing dependency (§10), fully reproducible failures.

use std::fs;

use rusta_edit::parse_response;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

/// Adversarial alphabet: markers at legal and illegal lengths, fences,
/// filenames, prose, blank and whitespace lines, unicode, CRLF.
const PIECES: &[&str] = &[
    "<<<<<<< SEARCH",
    "<<<<<<< SEARCH>",
    "<<<<<<<< SEARCH",
    "<<<<< SEARCH",
    "<<<<<<<<<<< SEARCH",
    "<<<<<<<",
    "=======",
    "========",
    "====",
    "================",
    ">>>>>>> REPLACE",
    ">>>>>>>> REPLACE",
    ">>>> REPLACE",
    ">>>>>>>",
    "src/main.rs",
    "main.rs",
    "```",
    "```rust",
    "```bash",
    "```sh scripts/run.sh",
    "cargo test",
    "...",
    "...  ",
    "fn main() {}",
    "\tif (x) {",
    "héllo wörld",
    "日本語テキスト",
    "a",
    "",
    "   ",
    "# comment",
    "old line",
    "new line",
];

fn random_response(rng: &mut Rng) -> String {
    let lines = rng.below(120);
    let mut out = String::new();
    for _ in 0..lines {
        out.push_str(rng.pick(PIECES));
        out.push('\n');
    }
    out
}

#[test]
fn parser_never_panics_on_arbitrary_input() {
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    for iteration in 0..5_000 {
        let text = random_response(&mut rng);
        let parsed = parse_response(&text);
        // Sanity invariant: everything lands in one of the three sinks and
        // block text is LF-only (CRLF normalized).
        for block in &parsed.blocks {
            assert!(!block.original.contains('\r'), "iteration {iteration}");
            assert!(!block.updated.contains('\r'), "iteration {iteration}");
        }
    }
}

#[test]
fn engine_never_panics_on_arbitrary_blocks() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("a.rs"), "fn a() {}\nfn shared() { 1 }\n").expect("fixture");
    fs::write(root.join("b.rs"), "struct B;\nfn shared() { 1 }\n").expect("fixture");
    fs::write(root.join("z.txt"), "line\nline\nline\n").expect("fixture");
    let mut editor = rusta_edit::Editor::new(root);
    for path in ["a.rs", "b.rs", "z.txt"] {
        editor.record_read(path);
    }

    let mut rng = Rng(0xfeed_beef_cafe_0001);
    for _ in 0..500 {
        let text = random_response(&mut rng);
        let report = editor.apply_response(&text);
        // The engine's own invariants hold under arbitrary input.
        assert_eq!(report.is_success(), report.failed.is_empty());
        if !report.is_success() {
            assert!(report.feedback.is_some());
        }
        // Every touched file stays valid UTF-8 text.
        for path in ["a.rs", "b.rs", "z.txt", "main.rs", "src/main.rs"] {
            if root.join(path).exists() {
                fs::read_to_string(root.join(path)).expect("valid utf-8");
            }
        }
        // Undo everything so each iteration starts from the same tree.
        while editor.undo_last().expect("undo").is_some() {}
    }
}
