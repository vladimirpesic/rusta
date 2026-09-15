//! The §6.9 slash commands. Every command is a plain function on [`App`];
//! `run` parses `/name args` and dispatches. Output goes through the
//! [`crate::render::Reporter`] so tests assert exactly what a user sees.

use std::path::Path;

use rusta_core::Status;
use rusta_tools::glob_match;

use crate::repl::{App, Control};

/// The §6.9 command table, `/help` text, and the unknown-command remedy.
const HELP: &str = "commands: /add <glob> /drop [<glob>] /undo /diff /map /state /auto \
     /model /backend /skills [<name>] /resume <file> /save /exit";

/// Parses and runs one `/command` (the leading `/` already stripped).
pub(crate) async fn run(app: &mut App, command: &str) -> Control {
    let mut parts = command.split_whitespace();
    let name = parts.next().unwrap_or_default();
    let rest: Vec<&str> = parts.collect();
    let arg = rest.join(" ");
    match name {
        "add" => add(app, &arg),
        "drop" => drop_files(app, &arg),
        "undo" => undo(app),
        "diff" => {
            app.reporter.line(&app.git.diff());
            Control::Continue
        }
        "map" => map(app),
        "state" => state(app),
        "auto" => {
            let on = !app.auto();
            app.set_auto(on);
            app.reporter.line(if on {
                "auto-approve ON (plan gate + shell)"
            } else {
                "auto-approve OFF"
            });
            Control::Continue
        }
        "model" => model(app),
        "backend" => backend(app),
        "skills" => skills(app, &arg),
        "resume" => resume(app, &arg),
        "save" => {
            app.reporter.line(&format!(
                "session saved continuously: {} ({} events) — the log is append-only",
                app.session.path().display(),
                app.session.events().len()
            ));
            Control::Continue
        }
        "exit" | "quit" => {
            app.reporter.line("bye");
            Control::Exit
        }
        "help" | "" => {
            app.reporter.line(HELP);
            Control::Continue
        }
        other => {
            app.reporter.line(&format!("unknown command /{other}"));
            app.reporter.line(HELP);
            Control::Continue
        }
    }
}

/// `/add <glob>` — adds matching files to the session chat-set (§6.9): the
/// read-before-edit ledger, which also steers repo-map ranking (§6.5).
/// Chat files never render in the map, so the model is told about them
/// directly via a journaled observation; it reads the content on demand.
fn add(app: &mut App, pattern: &str) -> Control {
    if pattern.is_empty() {
        app.reporter
            .line("usage: /add <path|dir|glob>  e.g. /add src  ·  /add '**/*.rs'");
        return Control::Continue;
    }
    let matched = walk_matching(&app.root, pattern);
    if matched.is_empty() {
        app.reporter.line(&format!("no files match {pattern}"));
        // `*.rs` is root-level here, as in a shell. Point at the recursive
        // form instead of leaving the user at a silent dead end.
        if let Some(hint) = recursive_hint(&app.root, pattern) {
            app.reporter.line(&hint);
        }
        return Control::Continue;
    }
    {
        let mut editor = app.tools.editor();
        for rel in &matched {
            editor.record_read(rel);
        }
    }
    // The map never renders session files (§6.5 step 5), so /add must tell
    // the model itself what just joined the set — a compact, journaled
    // observation. Content still enters context on demand via reads.
    let listed = matched
        .iter()
        .take(20)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let mut note = format!(
        "/add: the user added {} file(s) to the session set: {listed}",
        matched.len()
    );
    if matched.len() > 20 {
        note.push_str(&format!(", … and {} more", matched.len() - 20));
    }
    // Journaled as an `add` call carrying the credited paths, so `/resume`
    // rebuilds the same chat-set (§6.10). Under the old `"session"` name
    // replay credited nothing and the set vanished across a restart, while
    // the model's note about it replayed regardless.
    app.push_observation_with_input(
        "add",
        serde_json::json!({ "paths": matched }),
        &note,
        Status::Ok,
    );
    app.reporter
        .line(&format!("added {} file(s):", matched.len()));
    for rel in matched.iter().take(20) {
        app.reporter.line(&format!("  {rel}"));
    }
    if matched.len() > 20 {
        app.reporter
            .line(&format!("  … and {} more", matched.len() - 20));
    }
    Control::Continue
}

/// `/drop [<glob>]` — removes files from the chat-set; bare `/drop` lists it.
fn drop_files(app: &mut App, pattern: &str) -> Control {
    if pattern.is_empty() {
        let files: Vec<String> = {
            let editor = app.tools.editor();
            editor
                .ledger()
                .read_set()
                .map(|p| p.display().to_string())
                .collect()
        };
        if files.is_empty() {
            app.reporter.line("chat-set is empty");
        } else {
            app.reporter.line(&format!("chat-set ({}):", files.len()));
            for file in files {
                app.reporter.line(&format!("  {file}"));
            }
        }
        return Control::Continue;
    }
    // Same dialect as `/add` (see `walk_matching`); the chat-set is small
    // and `/drop` with no argument lists it, so there is no silent dead end
    // here to compensate for.
    let dropped: Vec<String> = {
        let mut editor = app.tools.editor();
        let matched: Vec<String> = editor
            .ledger()
            .read_set()
            .map(|p| p.display().to_string())
            .filter(|display| glob_match(pattern, display))
            .collect();
        matched
            .iter()
            .map(|display| {
                editor.drop_read(display);
                display.clone()
            })
            .collect()
    };
    if dropped.is_empty() {
        app.reporter
            .line(&format!("nothing in the chat-set matches {pattern}"));
    } else {
        app.reporter
            .line(&format!("dropped {} file(s)", dropped.len()));
    }
    Control::Continue
}

/// `/undo` (§6.9): restore the last batch's files from the journal, then
/// revert its commit — only if that commit is still exactly `HEAD`.
fn undo(app: &mut App) -> Control {
    let Some(batch) = app.batches.pop() else {
        app.reporter.line("nothing to undo");
        return Control::Continue;
    };
    let mut restored: Vec<String> = Vec::new();
    for _ in 0..batch.entries {
        match app.tools.editor().undo_last() {
            Ok(Some(entry)) => restored.push(entry.path.display().to_string()),
            Ok(None) => break,
            Err(err) => {
                app.reporter.line(&format!("undo failed on disk: {err}"));
                break;
            }
        }
    }
    // §6.9: journal the undo so replay cannot resurrect what it removed.
    // Recording the count actually restored (not `batch.entries`) keeps the
    // tombstone true even when the journal ran short.
    app.journal(rusta_core::Event::UndoApplied {
        entries: restored.len(),
    });
    app.reporter
        .line(&format!("restored {} file(s)", restored.len()));
    for path in &restored {
        app.reporter.line(&format!("  {path}"));
    }
    match &batch.sha {
        Some(sha) if app.git.reset_if_head(sha) => {
            app.reporter
                .line(&format!("reverted commit {}", short(sha)));
        }
        Some(sha) => {
            app.reporter.line(&format!(
                "commit {} kept — HEAD moved on after it (never reset unrelated commits)",
                short(sha)
            ));
        }
        None => {
            app.reporter
                .line("no commit to revert (not a git repository, or the commit failed)");
        }
    }
    Control::Continue
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// `/map` — render the repo map (§6.5), chat-set steering included.
fn map(app: &mut App) -> Control {
    let chat: Vec<String> = {
        let editor = app.tools.editor();
        editor
            .ledger()
            .read_set()
            .map(|p| p.display().to_string())
            .collect()
    };
    // The same §6.5 mention steering the model's `map_refresh` gets.
    let mentions = rusta_tools::mentioned_identifiers(&app.last_request);
    let rendered = app
        .tools
        .repomap()
        .render_map(&chat, None, &mentions, &mentions);
    app.reporter.line(if rendered.is_empty() {
        rusta_tools::EMPTY_MAP
    } else {
        &rendered
    });
    Control::Continue
}

/// `/state` — the phase machine, its registry, the repair budget, and the
/// undo stack (§6.9 "state + tool matrix").
fn state(app: &mut App) -> Control {
    let machine_state = app.machine.state();
    app.reporter.line(&format!(
        "state: {} — exit gate: {}",
        machine_state.name(),
        machine_state.exit_gate()
    ));
    let tools: Vec<&str> = machine_state.tools().iter().map(|t| t.as_str()).collect();
    app.reporter.line(&format!("tools: {}", tools.join(", ")));
    app.reporter.line(&format!(
        "repair budget: {} of {} used",
        app.gate.repairs_used(),
        rusta_validate::REPAIR_BOUND
    ));
    app.reporter.line(&format!(
        "undo stack: {} batch(es), {} edit(s)",
        app.batches.len(),
        app.batches.iter().map(|b| b.entries).sum::<usize>()
    ));
    app.reporter
        .line(&format!("session: {}", app.session.path().display()));
    Control::Continue
}

/// `/model` — the effective sampling settings. Selection is a start-time
/// decision (§6.2 "config or `--backend`"); this shows what is live.
fn model(app: &mut App) -> Control {
    app.reporter.line(&format!(
        "context window: {} tokens (backend-exact for embedded)",
        app.tools.backend().context_window()
    ));
    app.reporter.line(&format!(
        "max_tokens: {}  temperature: {}",
        app.config.model.max_tokens, app.config.model.temperature
    ));
    app.reporter
        .line("change [model] in rusta.toml or pass --model at launch");
    Control::Continue
}

/// `/backend` — the live backend kind and endpoint (§6.2).
fn backend(app: &mut App) -> Control {
    let kind = app.tools.backend().kind();
    app.reporter.line(&format!("backend: {kind:?}"));
    if matches!(kind, rusta_llm::BackendKind::Http) {
        let url = app
            .config
            .backend
            .base_url
            .clone()
            .unwrap_or_else(|| "http://127.0.0.1:8080/v1 (default)".to_owned());
        app.reporter.line(&format!("base_url: {url}"));
    } else {
        let path = app
            .config
            .backend
            .embedded
            .model_path
            .clone()
            .unwrap_or_else(|| "(unset)".to_owned());
        app.reporter.line(&format!("model_path: {path}"));
    }
    app.reporter
        .line("change [backend] in rusta.toml or pass --backend at launch");
    Control::Continue
}

/// `/skills [<name>]` — list the deck (§6.6) or inject a user-invocable card
/// (§6.9) into the conversation.
fn skills(app: &mut App, arg: &str) -> Control {
    if arg.is_empty() {
        let cards = app.deck.cards();
        if cards.is_empty() {
            app.reporter
                .line("no skill cards (drop *.md files in .rusta/skills/)");
            return Control::Continue;
        }
        app.reporter.line(&format!("{} card(s):", cards.len()));
        for card in cards {
            let invocable = if card.user_invocable() { " *" } else { "" };
            app.reporter.line(&format!(
                "  {} [{}]{} — triggers: {}",
                card.name(),
                card.kind().as_str(),
                invocable,
                card.triggers().join(", ")
            ));
        }
        app.reporter.line("* = user-invocable via /skills <name>");
        return Control::Continue;
    }
    // Take what the card owns before journaling: `journal` needs `&mut app`
    // and the card borrows `app.deck`.
    let Some((card_name, body)) = app.deck.invocable(arg).map(|card| {
        (
            card.name().to_owned(),
            format!("SKILL CARD {}:\n{}", card.name(), card.body()),
        )
    }) else {
        app.reporter.line(&format!(
            "no user-invocable card named {arg:?} (see /skills for the deck)"
        ));
        return Control::Continue;
    };
    app.journal(rusta_core::Event::UserMessage {
        content: body.clone(),
    });
    app.history.push(rusta_llm::Message::user(body));
    app.reporter
        .line(&format!("injected skill card {card_name}"));
    Control::Continue
}

/// `/resume <file>` — adopt a recorded session (§6.10).
fn resume(app: &mut App, arg: &str) -> Control {
    if arg.is_empty() {
        app.reporter.line("usage: /resume <session.jsonl>");
        return Control::Continue;
    }
    match app.resume_from(Path::new(arg)) {
        Ok(()) => {
            app.reporter.line(&format!(
                "resumed {} ({} messages, {} undo entries, phase {})",
                arg,
                app.history.len(),
                app.tools.editor().undo_stack().len(),
                app.machine.state()
            ));
        }
        Err(err) => app.reporter.line(&format!("resume failed: {err}")),
    }
    Control::Continue
}

// -------------------------------------------------------------- glob support

/// Repo-relative paths under `root` selected by `pattern`, capped at the
/// §6.1 glob limit.
///
/// **Path semantics, like a shell and like Aider's `/add`** (which globs
/// with `Path(root).glob(pattern)`): `*.rs` matches at the repo root and
/// `**/*.rs` recurses. A bare **directory** expands to its whole subtree —
/// Aider's `expand_subdir`, and the ordinary way to add a package without
/// knowing glob syntax at all.
///
/// The model-facing `glob` tool deliberately differs: it normalizes a
/// slash-free pattern to `**/pattern`, gitignore-style, because a small
/// model that writes `glob("*.rs")` means "find the Rust files" and cannot
/// see the result to correct it. Here the user can, so least-surprise wins;
/// when a slash-free pattern selects nothing, [`add`] offers the recursive
/// form rather than expanding silently and sweeping in hundreds of files.
/// The two share a walker and an ignore set, not a pattern dialect.
///
/// Dot-entries are skipped: the model may legitimately want
/// `.github/workflows`, but sweeping hidden files into the chat-set by glob
/// is almost never what a user means.
fn walk_matching(root: &Path, pattern: &str) -> Vec<String> {
    const CAP: usize = 1_000;
    let subtree = directory_prefix(root, pattern);
    let mut out: Vec<String> = rusta_tools::walk(root)
        .iter()
        .map(|rel| rusta_tools::display(rel))
        .filter(|rel| !rel.split('/').any(|part| part.starts_with('.')))
        .filter(|rel| match &subtree {
            Some(prefix) => rel.starts_with(prefix.as_str()),
            None => glob_match(pattern, rel),
        })
        .take(CAP)
        .collect();
    out.sort();
    out
}

/// `Some("src/")` when `pattern` names an existing directory in the repo —
/// the subtree form. Glob metacharacters and `..` never take this path, and
/// the prefix is only ever matched against repo-relative walk output, so it
/// cannot select anything outside the root.
fn directory_prefix(root: &Path, pattern: &str) -> Option<String> {
    if pattern.contains(['*', '?', '[']) {
        return None;
    }
    // Walk output is repo-relative with no `./`, so the prefix must be too:
    // `/add .` and `/add ./src` built the prefixes `./` and `./src/` and so
    // matched nothing at all — in the very command whose directory form had
    // just been advertised.
    let trimmed = pattern
        .trim_end_matches('/')
        .trim_start_matches("./")
        .trim_end_matches('/');
    if trimmed.split('/').any(|part| part == "..") {
        return None;
    }
    // `.` — or a bare `./` — is the repo root: an empty prefix, which every
    // repo-relative path starts with.
    if trimmed.is_empty() || trimmed == "." {
        return root.is_dir().then(String::new);
    }
    root.join(trimmed).is_dir().then(|| format!("{trimmed}/"))
}

/// For a slash-free pattern that selected nothing, the recursive form and
/// how many files it would select — `None` when that finds nothing either,
/// so the hint is never a second dead end.
fn recursive_hint(root: &Path, pattern: &str) -> Option<String> {
    if pattern.contains('/') || !pattern.contains(['*', '?', '[']) {
        return None;
    }
    let recursive = format!("**/{pattern}");
    let count = walk_matching(root, &recursive).len();
    (count > 0).then(|| format!("  did you mean /add {recursive}?  ({count} file(s))"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_star_stops_at_separators_while_double_star_crosses() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"), "* does not cross '/'");
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(
            glob_match("**/*.rs", "main.rs"),
            "** may match zero segments"
        );
        assert!(glob_match("src/**/*.rs", "src/a/b/c.rs"));
        assert!(glob_match("src/**", "src/a/b"));
        assert!(glob_match("a?c.rs", "abc.rs"));
        assert!(!glob_match("a?c.rs", "ac.rs"));
        assert!(glob_match("exactly/one.txt", "exactly/one.txt"));
    }

    #[test]
    fn walk_matching_finds_nested_sources_and_skips_hidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        for rel in ["src/lib.rs", "src/deep/mod.rs", "README.md", ".hidden/x.rs"] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, "x\n").expect("write");
        }
        std::fs::create_dir_all(root.join("target/debug")).expect("mkdir");
        std::fs::write(root.join("target/debug/skip.rs"), "x\n").expect("write");
        std::fs::create_dir_all(root.join("dist")).expect("mkdir");
        std::fs::write(root.join("dist/gen.rs"), "x\n").expect("write");

        let found = walk_matching(root, "**/*.rs");
        assert_eq!(
            found,
            ["src/deep/mod.rs", "src/lib.rs"],
            "sorted, no hidden, no build output (dist skips like the map)"
        );
        assert_eq!(walk_matching(root, "*.md"), ["README.md"]);
        assert!(walk_matching(root, "*.toml").is_empty());
    }
}
