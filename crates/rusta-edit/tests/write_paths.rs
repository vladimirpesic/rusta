//! The structural guard for DEVELOPMENT_PLAN.md §6.12 confinement.
//!
//! The second audit fenced "three write paths"; there were four, and the
//! uncounted one (`UndoStack::undo_last`) carried a delete as well as a
//! write, reachable through `/resume`. Hand-enumeration has now failed
//! twice, so this test does the counting: it scans the crate's production
//! source for raw mutation calls and fails when one appears outside the
//! single guarded helper.
//!
//! A detector is only as good as what it looks for. This one shipped
//! covering four primitives and missing five — `File::create`, `fs::rename`,
//! `fs::copy`, `fs::create_dir`, `OpenOptions` — so a probe adding five
//! unfenced writes left it green while it claimed the opposite. It now
//! carries a proof-of-life assertion (below) so it cannot pass by failing to
//! look, and its own coverage is exercised by injecting the calls it must
//! catch rather than only by the calls that already existed.

use std::path::Path;

/// Raw mutation primitives, each of which must live inside `apply::guarded`.
///
/// The list was previously four entries and missed `File::create`,
/// `fs::rename`, `fs::copy`, `fs::create_dir` (only the `_all` form was
/// listed), `OpenOptions`, `set_permissions`, `hard_link` and `symlink` — a
/// probe that added five unfenced writes using them left this test green
/// while both this module and `apply.rs` claimed it could not. Anything that
/// can create, replace, move, delete or re-permission a path belongs here.
const MUTATORS: [&str; 12] = [
    "fs::write(",
    "fs::remove_file(",
    "fs::remove_dir",
    "fs::create_dir(",
    "fs::create_dir_all(",
    "fs::rename(",
    "fs::copy(",
    "fs::hard_link(",
    "fs::set_permissions(",
    "fs::soft_link(",
    "File::create(",
    "OpenOptions::",
];

/// Raw mutation calls that `guarded` itself is expected to contain. If the
/// scan stops finding these, it has stopped working — a renamed helper, a
/// renamed file, or desynced brace tracking — and would otherwise pass by
/// not looking. A detector must be able to tell "nothing is wrong" from
/// "I am not looking".
const EXPECTED_INSIDE_GUARDED: usize = 3;

/// The one function allowed to call them.
const GUARDED_FN: &str = "fn guarded(";

/// Every `.rs` file under `dir`, recursively — a submodule directory added
/// later must not slip out of the scan simply by existing.
fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("src dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_mutation_goes_through_the_guarded_helper() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<String> = Vec::new();
    let mut found_guarded = false;
    let mut calls_inside_guarded = 0usize;

    let mut files = Vec::new();
    sources(&src, &mut files);
    assert!(!files.is_empty(), "the scan found no source files at all");

    for path in files {
        let text = std::fs::read_to_string(&path).expect("read");
        // Production only: everything from `#[cfg(test)]` down is fixtures.
        let production = text
            .split_once("\n#[cfg(test)]")
            .map_or(text.as_str(), |(head, _)| head);

        let mut inside_guarded = false;
        let mut brace_depth = 0i32;
        for (index, line) in production.lines().enumerate() {
            if line.contains(GUARDED_FN) {
                inside_guarded = true;
                found_guarded = true;
                brace_depth = 0;
            }
            if inside_guarded {
                brace_depth += line.matches('{').count() as i32;
                brace_depth -= line.matches('}').count() as i32;
            }
            let is_call = MUTATORS.iter().any(|m| line.contains(m));
            let is_comment = line.trim_start().starts_with("//");
            if is_call && !is_comment {
                if inside_guarded {
                    calls_inside_guarded += 1;
                } else {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        index + 1,
                        line.trim()
                    ));
                }
            }
            if inside_guarded && brace_depth <= 0 && line.contains('}') {
                inside_guarded = false;
            }
        }
    }

    // Proof of life, before the real assertion: a detector that has stopped
    // detecting must fail loudly rather than pass by finding nothing.
    assert!(
        found_guarded,
        "`{GUARDED_FN}` was not located — the scan is not looking where it thinks it is"
    );
    assert_eq!(
        calls_inside_guarded, EXPECTED_INSIDE_GUARDED,
        "expected {EXPECTED_INSIDE_GUARDED} raw mutation calls inside `guarded`, saw \
         {calls_inside_guarded}. Either the helper changed (update the constant) or the \
         scan has desynced and is no longer seeing what it must see."
    );

    assert!(
        offenders.is_empty(),
        "mutation calls outside `apply::guarded` — each one is an unfenced \
         write path, which is how §6.12 confinement was missed twice:\n  {}",
        offenders.join("\n  ")
    );
}
