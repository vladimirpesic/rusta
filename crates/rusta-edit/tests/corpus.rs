//! Parser fixture corpus — M2 acceptance (ADR.md §8): Aider
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
    // A1's shape, added in round 8: the model ran the REPLACE text straight
    // onto the UPDATED marker. Aider rejects this outright; §6.3 rule 4
    // forgives it *after* stripping the marker, because forgiving it without
    // the strip wrote `>>>>>>> REPLACE` into the user's source and
    // auto-committed it. The corpus held no fixture of this shape, so when
    // the strip was disabled as a probe neither corpus test noticed — only
    // the dedicated unit test did.
    Case {
        file: "16_glued_marker.md",
        blocks: &[(
            &["src/main.rs"],
            "fn main() {\n    println!(\"old\");\n}\n",
            "fn main() {\n    println!(\"new\");\n}\n",
        )],
        commands: &[],
        notes: 1,
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

// ------------------------------------------- 2026-09-14 third-audit findings

/// F13: §6.12 confinement must hold at the *filesystem* level, not only
/// lexically. `confine` rejects `..` and absolute spellings, but a symlink
/// committed inside the repo and pointing out of it was followed by
/// `fs::write`, so an in-repo-looking path wrote outside the workspace.
#[test]
fn a_symlink_out_of_the_repo_is_not_a_write_target() {
    let repo = tempfile::tempdir().expect("repo");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("victim.txt"), "ORIGINAL\n").expect("seed");
    std::os::unix::fs::symlink(outside.path(), repo.path().join("escape")).expect("symlink");

    let mut editor = rusta_edit::Editor::new(repo.path());
    editor.record_read("escape/victim.txt");
    let report = editor.apply_response(
        "escape/victim.txt\n<<<<<<< SEARCH\nORIGINAL\n=======\nOVERWRITTEN\n>>>>>>> REPLACE\n",
    );

    assert!(report.applied.is_empty(), "the write must be refused");
    assert_eq!(
        std::fs::read_to_string(outside.path().join("victim.txt")).expect("read"),
        "ORIGINAL\n",
        "a file outside the repo was modified"
    );
    assert!(matches!(
        report.failed[0].reason,
        rusta_edit::FailureReason::OutsideRoot(_)
    ));
}

/// F13 control: ordinary in-repo edits are untouched by the containment
/// check, including creating a file that does not exist yet.
#[test]
fn repo_local_edits_and_creates_still_apply() {
    let repo = tempfile::tempdir().expect("repo");
    std::fs::write(repo.path().join("a.txt"), "ORIGINAL\n").expect("seed");
    let mut editor = rusta_edit::Editor::new(repo.path());
    editor.record_read("a.txt");

    let edited = editor
        .apply_response("a.txt\n<<<<<<< SEARCH\nORIGINAL\n=======\nCHANGED\n>>>>>>> REPLACE\n");
    assert_eq!(edited.applied.len(), 1, "{:?}", edited.failed);

    let created =
        editor.apply_response("new/deep/b.txt\n<<<<<<< SEARCH\n=======\nfresh\n>>>>>>> REPLACE\n");
    assert_eq!(created.applied.len(), 1, "{:?}", created.failed);
    assert!(repo.path().join("new/deep/b.txt").exists());
}

/// F11: shell-fence detection matches the language token whole. Prefix
/// matching made ` ```csharp ` a suggested shell command (via `csh`), and
/// likewise ` ```shader ` and ` ```batchfile `.
#[test]
fn code_fences_are_not_mistaken_for_shell_commands() {
    for language in ["csharp", "shader", "batchfile", "rust", "python"] {
        let parsed = rusta_edit::parse_response(&format!("```{language}\nlet x = 1;\n```\n"));
        assert!(
            parsed.commands.is_empty(),
            "```{language} was read as a shell command: {:?}",
            parsed.commands
        );
    }
    // Real shell fences still are.
    for language in ["bash", "sh", "shell", "zsh"] {
        let parsed = rusta_edit::parse_response(&format!("```{language}\ncargo test\n```\n"));
        assert_eq!(parsed.commands, ["cargo test".to_owned()], "```{language}");
    }
}

// ------------------------------------- 2026-09-15 consolidated-audit findings

/// A1: §6.3 rule 4's marker strip must run on *every* commit out of the
/// REPLACE state, not only at end-of-stream.
///
/// A block closed mid-stream — by a following UPDATED line, or by a chained
/// DIVIDER — used to commit `…new>>>>>>> REPLACE` verbatim, writing the
/// marker into the user's source while reporting the apply as successful and
/// emitting no corrective note. Any completion carrying more than one edit
/// block reaches that path.
#[test]
fn a_glued_replace_marker_is_stripped_at_every_commit() {
    // (a) end-of-stream — the case rule 4's erratum already covered.
    let eos = "a.rs\n<<<<<<< SEARCH\nold\n=======\nnew>>>>>>> REPLACE\n";
    let parsed = rusta_edit::parse_response(eos);
    assert_eq!(parsed.blocks[0].updated, "new\n");
    assert_eq!(parsed.notes.len(), 1, "end-of-stream must warn");

    // (b) mid-stream, closed by a following UPDATED line.
    let mid = "a.rs\n<<<<<<< SEARCH\nold\n=======\nnew>>>>>>> REPLACE\n>>>>>>> REPLACE\n";
    let parsed = rusta_edit::parse_response(mid);
    assert_eq!(
        parsed.blocks[0].updated, "new\n",
        "the marker must never reach the file"
    );
    assert_eq!(parsed.notes.len(), 1, "mid-stream must warn too");

    // (c) mid-stream, closed by a chained DIVIDER (§6.3 rule 1).
    let chained = "a.rs\n<<<<<<< SEARCH\nold\n=======\nnew>>>>>>> REPLACE\n=======\nthree\n=======\nfour\n>>>>>>> REPLACE\n";
    let parsed = rusta_edit::parse_response(chained);
    assert_eq!(parsed.blocks[0].updated, "new\n");
    assert_eq!(parsed.blocks.len(), 2, "chaining still works");
    assert_eq!(parsed.blocks[1].original, "three\n");
    assert!(parsed.notes.iter().any(|n| n.contains(">>>>>>> REPLACE")));
}

/// A1 control: well-formed blocks are untouched by the strip, and REPLACE
/// text that merely *ends* in angle brackets survives verbatim.
#[test]
fn well_formed_blocks_are_unaffected_by_the_marker_strip() {
    let clean = "a.rs\n<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\nb.rs\n<<<<<<< SEARCH\np\n=======\nq\n>>>>>>> REPLACE\n";
    let parsed = rusta_edit::parse_response(clean);
    assert_eq!(parsed.blocks.len(), 2);
    assert_eq!(parsed.blocks[0].updated, "new\n");
    assert_eq!(parsed.blocks[1].updated, "q\n");
    assert!(parsed.notes.is_empty(), "{:?}", parsed.notes);

    let generics = "a.rs\n<<<<<<< SEARCH\nold\n=======\nVec<Vec<u8>>\n>>>>>>> REPLACE\n";
    let parsed = rusta_edit::parse_response(generics);
    assert_eq!(parsed.blocks[0].updated, "Vec<Vec<u8>>\n");
    assert!(parsed.notes.is_empty());
}

/// Fixtures whose blocks cannot be exercised by seeding the named file with
/// the SEARCH text verbatim, and why. Keeping the reasons here rather than
/// silently filtering is the point: an unexplained exclusion is how a corpus
/// quietly stops covering what it claims to.
const NOT_SEED_APPLIABLE: &[(&str, &str)] = &[
    (
        "12_dotdotdots.md",
        "`...` elision: SEARCH stands in for a larger file, so seeding it \
         verbatim is not the case under test",
    ),
    (
        "13_malformed_missing_divider.md",
        "no blocks — degrades to prose plus a corrective note",
    ),
    ("14_prose_only.md", "no blocks"),
];

/// A41 (round 8): ADR §9 states each corpus fixture carries "an expected
/// parse **and** an expected apply result". Only the parse half existed —
/// `parser_corpus_is_green` never touched `Editor`, so the apply chain was
/// covered by hand-written unit fixtures alone and never by the real
/// small-model response shapes this corpus exists to hold.
///
/// The expectation is derived rather than hand-written: seed each named file
/// with the first block's SEARCH, apply the fixture text through the real
/// chain, and require the file to end as the last block's REPLACE. That is
/// not tautological — it runs `prep`, fence stripping, whitespace
/// flexibility, CRLF normalization and the marker strip, which is precisely
/// where this crate's defects have lived (a glued `>>>>>>> REPLACE` in the
/// committed text would fail here, and did not fail the parse-only test).
#[test]
fn corpus_fixtures_apply_as_well_as_parse() {
    let mut exercised = 0usize;
    for case in CASES {
        if let Some((_, why)) = NOT_SEED_APPLIABLE.iter().find(|(f, _)| *f == case.file) {
            assert!(!why.is_empty());
            continue;
        }
        if case.blocks.is_empty() {
            continue;
        }
        let text = std::fs::read_to_string(format!("{CORPUS_DIR}/{}", case.file))
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", case.file));

        let repo = tempfile::tempdir().expect("repo");
        let mut editor = rusta_edit::Editor::new(repo.path());

        // A fixture's blocks touch disjoint regions of one file (both
        // multi-block fixtures are that shape, not a chain), so the file
        // before is every SEARCH concatenated in order and the file after is
        // every REPLACE. For a single block this is just seed = SEARCH,
        // expect = REPLACE; for an empty SEARCH it is a create, so nothing
        // is seeded.
        let mut before: std::collections::BTreeMap<&str, String> = Default::default();
        let mut after: std::collections::BTreeMap<&str, String> = Default::default();
        // A block with no candidates is a §6.3 continuation: it reuses the
        // previous block's filename. Carrying it forward here is the same
        // rule the parser applies, and without it the chained-divider
        // fixture seeds only its first region and the rest cannot match.
        let mut current: Option<&str> = None;
        for (candidates, original, updated) in case.blocks {
            if let Some(path) = candidates.first() {
                current = Some(path);
            }
            let Some(path) = current else {
                continue;
            };
            before.entry(path).or_default().push_str(original);
            after.entry(path).or_default().push_str(updated);
        }
        for (path, seed) in &before {
            if seed.is_empty() {
                continue; // create-file block
            }
            let abs = repo.path().join(path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).expect("parent");
            }
            std::fs::write(&abs, seed).expect("seed");
            editor.record_read(path);
        }

        let report = editor.apply_response(&text);
        assert!(
            report.failed.is_empty(),
            "{}: every block in a well-formed fixture must apply: {:?}",
            case.file,
            report.failed
        );
        assert_eq!(
            report.applied.len(),
            case.blocks.len(),
            "{}: applied count must match the parsed block count",
            case.file
        );
        for (path, expected) in &after {
            let on_disk = std::fs::read_to_string(repo.path().join(path))
                .unwrap_or_else(|e| panic!("{}: read {path} after apply: {e}", case.file));
            assert_eq!(
                on_disk, *expected,
                "{}: {path} must end as the fixture's REPLACE text",
                case.file
            );
        }
        exercised += 1;
    }
    assert!(
        exercised >= 10,
        "the apply pass must actually exercise the corpus, not skip it: {exercised} fixtures"
    );
}
