//! The structural guard for DEVELOPMENT_PLAN.md §6.12 confinement.
//!
//! The second audit fenced "three write paths"; there were four, and the
//! uncounted one (`UndoStack::undo_last`) carried a delete as well as a
//! write, reachable through `/resume`. Hand-enumeration has now failed
//! twice, so this test does the counting: it scans the crate's production
//! source for raw mutation calls and fails when one appears outside the
//! single guarded helper.

use std::path::Path;

/// Raw mutation calls, each of which must live inside `apply::guarded`.
const MUTATORS: [&str; 4] = [
    "fs::write(",
    "fs::remove_file(",
    "fs::remove_dir",
    "fs::create_dir_all(",
];

/// The one function allowed to call them.
const GUARDED_FN: &str = "fn guarded(";

#[test]
fn every_mutation_goes_through_the_guarded_helper() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<String> = Vec::new();

    for entry in std::fs::read_dir(&src).expect("src dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
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
                brace_depth = 0;
            }
            if inside_guarded {
                brace_depth += line.matches('{').count() as i32;
                brace_depth -= line.matches('}').count() as i32;
            }
            let is_call = MUTATORS.iter().any(|m| line.contains(m));
            let is_comment = line.trim_start().starts_with("//");
            if is_call && !is_comment && !inside_guarded {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    index + 1,
                    line.trim()
                ));
            }
            if inside_guarded && brace_depth <= 0 && line.contains('}') {
                inside_guarded = false;
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "mutation calls outside `apply::guarded` — each one is an unfenced \
         write path, which is how §6.12 confinement was missed twice:\n  {}",
        offenders.join("\n  ")
    );
}
