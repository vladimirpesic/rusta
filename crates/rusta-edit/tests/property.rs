//! Property test (M2 acceptance, ADR.md §8): the parser never
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
    // CRLF, which the doc claimed and the alphabet did not carry — so the
    // "block text is LF-only" invariant was vacuous for generated input.
    "old line\r\nnew line",
    "\r\n",
    "trailing cr\r",
    "main.rs",
    "/etc/passwd",
    "/tmp/escape.txt",
    "../escape.rs",
    "../../escape.rs",
    "src/../../escape.rs",
    "~/escape.rs",
    "./src/main.rs",
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
        // Sometimes omit the newline, so a marker can end up glued to the
        // end of a content line. That is an ordinary small-model slip and
        // the old alphabet could not express it.
        if rng.below(8) > 0 {
            out.push('\n');
        }
    }
    out
}

#[test]
fn parser_never_panics_on_arbitrary_input() {
    let mut rng = Rng(0x5eed_1234_abcd_ef01);
    for iteration in 0..5_000 {
        let text = random_response(&mut rng);
        let parsed = parse_response(&text);
        // Determinism: the same input must parse to the same result. This
        // is the invariant that actually holds over arbitrary input — and
        // it is the one that matters, since the apply chain, the undo
        // journal and session replay all assume a stable parse.
        //
        // Note on CRLF: the alphabet now carries `\r\n` (its doc claimed so
        // for rounds while the pieces were LF-only, making the old "no `\r`
        // in block text" assertion vacuous). That assertion cannot be
        // restored even in a weakened form: a content CR immediately before
        // a line break is textually identical to a CRLF ending, so
        // `"x\r" + CRLF` correctly normalizes to `"x\r\n"` and any blanket
        // CR check would fail on correct output. §6.3's CRLF contract is
        // pinned where it is actually expressible — `crlf_is_normalized`,
        // over realistic input.
        let again = parse_response(&text);
        assert_eq!(
            parsed, again,
            "iteration {iteration}: parse is not deterministic"
        );
    }
}

#[test]
fn engine_never_panics_on_arbitrary_blocks() {
    let base = tempfile::tempdir().expect("tempdir");
    // The repo is a *subdirectory*, so "outside the repo but inside the
    // fixture" is expressible — the containment invariant below needs
    // somewhere for an escape to land.
    let root = &base.path().join("repo");
    fs::create_dir_all(root).expect("repo dir");
    let outside = base.path().join("outside.txt");
    // Deliberately a line the alphabet emits: cross-file retry can only
    // select this file if some generated SEARCH body matches it, so the
    // invariant below is reachable rather than decorative.
    fs::write(&outside, "old line\n").expect("fixture");
    fs::write(root.join("a.rs"), "fn a() {}\nfn shared() { 1 }\n").expect("fixture");
    fs::write(root.join("b.rs"), "struct B;\nfn shared() { 1 }\n").expect("fixture");
    fs::write(root.join("z.txt"), "line\nline\nline\n").expect("fixture");
    let mut editor = rusta_edit::Editor::new(root);
    for path in ["a.rs", "b.rs", "z.txt"] {
        editor.record_read(path);
    }
    // A resumed session can carry an out-of-repo path into the ledger (old
    // logs recorded absolute paths). The ledger must refuse it, because
    // cross-file retry writes to whatever the read-set holds.
    editor.record_read(&outside.display().to_string());

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
        // §6.12: no mutation may land outside the repo root. This is the
        // invariant the suite never asserted, which is how an unconfined
        // edit pathway shipped in the first place.
        for applied in &report.applied {
            assert!(
                rusta_edit::Editor::new(root)
                    .root()
                    .join(&applied.path)
                    .starts_with(root),
                "edit escaped the repo root: {}",
                applied.path.display()
            );
        }
        assert_eq!(
            fs::read_to_string(&outside).expect("outside file"),
            "old line\n",
            "a file outside the repo root was modified"
        );
        assert!(
            !base.path().join("escape.rs").exists() && !base.path().join("escape.txt").exists(),
            "a file was created outside the repo root"
        );
        // Undo everything so each iteration starts from the same tree.
        while editor.undo_last().expect("undo").is_some() {}
    }
}
