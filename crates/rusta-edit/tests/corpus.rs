//! Parser fixture corpus — M2 acceptance (DEVELOPMENT_PLAN.md §8): Aider
//! fixture corpus + malformed-input corpus must parse green. Fixtures are
//! the real-response shapes small models produce (fenced filenames, chained
//! dividers, CRLF, flexible marker lengths, shell suggestions, malformed
//! blocks) and live in the workspace-level `tests/edit_corpus/` (§5).

use rusta_edit::parse_response;

const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/edit_corpus");

struct Case {
    file: &'static str,
    /// (candidates, original, updated) per expected block.
    blocks: &'static [(&'static [&'static str], &'static str, &'static str)],
    commands: &'static [&'static str],
    notes: usize,
}

const CASES: &[Case] = &[
    Case {
        file: "01_simple.md",
        blocks: &[(
            &["src/main.rs"],
            "fn main() {\n    println!(\"old\");\n}\n",
            "fn main() {\n    println!(\"new\");\n}\n",
        )],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "02_missing_final_updated.md",
        blocks: &[(
            &["src/main.rs"],
            "fn main() {\n    println!(\"old\");\n}\n",
            "fn main() {\n    println!(\"new\");\n}\n",
        )],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "03_fenced_filename.md",
        blocks: &[(
            &["word_count.py"],
            "def count(text):\n    return 0\n",
            "def count(text):\n    return len(text.split())\n",
        )],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "04_fenced_prefixed_filename.md",
        blocks: &[(
            &["src/lib.rs"],
            "pub fn version() -> &str { \"0.1\" }\n",
            "pub fn version() -> &str { \"0.2\" }\n",
        )],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "05_filename_decoration.md",
        blocks: &[(&["src/main.rs"], "old\n", "new\n")],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "06_chained_dividers.md",
        blocks: &[
            (
                &["src/lib.rs"],
                "fn one() {}\n",
                "fn one() { renamed(); }\n",
            ),
            (&[], "fn two() {}\n", "fn two() { renamed(); }\n"),
        ],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "07_new_file.md",
        blocks: &[(
            &["docs/NOTES.md"],
            "",
            "# Project notes\n\n- edits land here\n",
        )],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "08_crlf.md",
        blocks: &[(&["src/crlf.rs"], "old\n", "new\n")],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "09_shell_command.md",
        blocks: &[(
            &["src/main.rs"],
            "println!(\"hi\");\n",
            "println!(\"hello\");\n",
        )],
        commands: &["cargo test\ncargo clippy -- -D warnings"],
        notes: 0,
    },
    Case {
        file: "10_shell_prelude_not_command.md",
        blocks: &[(
            &["scripts/build.sh"],
            "cargo build\n",
            "cargo build --release\n",
        )],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "11_flexible_markers.md",
        blocks: &[(
            &["deeply/nested/mod.rs"],
            "const MAX: usize = 10;\n",
            "const MAX: usize = 20;\n",
        )],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "12_dotdotdots.md",
        blocks: &[(
            &["src/large.rs"],
            "fn head() {\n...\nfn tail() {\n    zero();\n}\n",
            "fn head() {\n...\nfn tail() {\n    one();\n}\n",
        )],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "13_malformed_missing_divider.md",
        blocks: &[],
        commands: &[],
        notes: 1,
    },
    Case {
        file: "14_prose_only.md",
        blocks: &[],
        commands: &[],
        notes: 0,
    },
    Case {
        file: "15_continuation.md",
        blocks: &[(&["src/alpha.rs"], "a1\n", "b1\n"), (&[], "a2\n", "b2\n")],
        commands: &[],
        notes: 0,
    },
];

#[test]
fn parser_corpus_is_green() {
    for case in CASES {
        let path = format!("{CORPUS_DIR}/{}", case.file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", case.file));
        let parsed = parse_response(&text);

        assert_eq!(
            parsed.blocks.len(),
            case.blocks.len(),
            "{}: block count",
            case.file
        );
        for (i, (candidates, original, updated)) in case.blocks.iter().enumerate() {
            assert_eq!(
                parsed.blocks[i].candidates,
                candidates.to_vec(),
                "{}: block {i} candidates",
                case.file
            );
            assert_eq!(
                &parsed.blocks[i].original, original,
                "{}: block {i} original",
                case.file
            );
            assert_eq!(
                &parsed.blocks[i].updated, updated,
                "{}: block {i} updated",
                case.file
            );
        }
        assert_eq!(
            parsed.commands,
            case.commands.to_vec(),
            "{}: commands",
            case.file
        );
        assert_eq!(parsed.notes.len(), case.notes, "{}: notes", case.file);
    }
}
