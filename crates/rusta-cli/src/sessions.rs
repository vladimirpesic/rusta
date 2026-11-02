//! `rusta sessions list` and `rusta fork`.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::session_store::{SessionEntry, SessionStore};

/// What `sessions list` shows.
pub struct ListOptions {
    /// Every session, wherever it ran.
    pub all: bool,
    /// Sessions that ran in this directory; the working directory otherwise.
    pub dir: PathBuf,
    /// At most this many rows.
    pub limit: Option<usize>,
    /// JSON rather than a table.
    pub json: bool,
}

/// `rusta sessions list`: the sessions that ran in a directory (or all of
/// them), most recent first.
pub async fn list(records_dir: PathBuf, options: &ListOptions) -> anyhow::Result<()> {
    let store = SessionStore::new(records_dir).await;
    let dir = canonical(&options.dir);
    let scope = if options.all { None } else { Some(&dir) };
    let rows: Vec<SessionEntry> = select(store.list(), scope)
        .into_iter()
        .take(options.limit.unwrap_or(usize::MAX))
        .collect();

    if options.json {
        println!("{}", serde_json::to_string_pretty(&json_rows(&rows))?);
        return Ok(());
    }
    if rows.is_empty() {
        if options.all {
            println!("No sessions yet. Start one with `rusta --session <id>`.");
        } else {
            println!(
                "No sessions ran in {}. `rusta sessions list --all` shows every session.",
                dir.display()
            );
        }
        return Ok(());
    }
    for line in table(&rows) {
        println!("{line}");
    }
    Ok(())
}

/// What `fork` copies, and into what.
pub struct ForkOptions {
    /// The session to copy; the latest one that ran in `dir` when `None`.
    pub source: Option<String>,
    /// Where to look for the latest session.
    pub dir: PathBuf,
    /// The new session's id; a fresh one when `None`.
    pub id: Option<String>,
    /// Fork without asking.
    pub yes: bool,
}

/// `rusta fork`: copy a session into a new one and say how to resume it.
/// Asks first unless `yes`.
pub async fn fork(records_dir: PathBuf, options: ForkOptions) -> anyhow::Result<()> {
    let mut store = SessionStore::new(records_dir).await;
    let original = match &options.source {
        Some(source) => store.get(source).ok_or_else(|| {
            anyhow::anyhow!("no session '{source}'; `rusta sessions list --all` lists them")
        })?,
        None => {
            let dir = canonical(&options.dir);
            select(store.list(), Some(&dir))
                .into_iter()
                .next()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no session ran in {}; name one (`rusta sessions list --all` lists them)",
                        dir.display()
                    )
                })?
        }
    };
    let source = original.id.clone();
    let yes = options.yes;
    let id = options.id.unwrap_or_else(|| {
        format!(
            "{source}-fork-{}",
            ulid::Ulid::generate().to_string().to_lowercase()
        )
    });
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("fork asks before copying a session; pass -y to fork without a terminal");
        }
        print!(
            "Fork session '{}' ({}) into '{id}'? [Y/n] ",
            original.id,
            clean(original.label())
        );
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if matches!(answer.trim().to_lowercase().as_str(), "n" | "no") {
            println!("Not forked.");
            return Ok(());
        }
    }
    let fork = store
        .fork(&source, &id)
        .await
        .map_err(|e| anyhow::anyhow!(e))?;
    println!("Forked '{source}' into '{}'.", fork.id);
    match &fork.cwd {
        Some(dir) => println!(
            "Resume it with: cd {} && rusta --session {} --resume",
            shell_quote(&dir.display().to_string()),
            shell_quote(&fork.id)
        ),
        None => println!(
            "Resume it with: rusta --session {} --resume",
            shell_quote(&fork.id)
        ),
    }
    Ok(())
}

/// The sessions that ran in `dir`, or every session when `dir` is `None`.
fn select(entries: Vec<SessionEntry>, dir: Option<&PathBuf>) -> Vec<SessionEntry> {
    match dir {
        None => entries,
        Some(dir) => entries
            .into_iter()
            .filter(|e| e.cwd.as_ref().is_some_and(|cwd| canonical(cwd) == *dir))
            .collect(),
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A field fit for one table cell: control characters (a newline in a title,
/// a terminal escape) become spaces.
fn clean(field: &str) -> String {
    field
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Rows aligned under a header: id, last activity, model, title.
fn table(rows: &[SessionEntry]) -> Vec<String> {
    let cells: Vec<[String; 4]> = rows
        .iter()
        .map(|e| {
            [
                clean(&e.id),
                e.last_active_at
                    .with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string(),
                clean(&format!("{}/{}", e.provider, e.model)),
                clean(e.label()),
            ]
        })
        .collect();
    let header = ["ID", "LAST ACTIVE", "MODEL", "TITLE"];
    let width = |i: usize| {
        cells
            .iter()
            .map(|row| row[i].chars().count())
            .chain(std::iter::once(header[i].len()))
            .max()
            .unwrap_or(0)
    };
    let widths = [width(0), width(1), width(2)];
    let line = |row: [&str; 4]| {
        format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        )
        .trim_end()
        .to_string()
    };
    std::iter::once(line(header))
        .chain(cells.iter().map(|row| {
            line([
                row[0].as_str(),
                row[1].as_str(),
                row[2].as_str(),
                row[3].as_str(),
            ])
        }))
        .collect()
}

fn json_rows(rows: &[SessionEntry]) -> serde_json::Value {
    serde_json::Value::Array(
        rows.iter()
            .map(|e| {
                serde_json::json!({
                    "id": clean(&e.id),
                    "title": clean(&e.display_title),
                    "cwd": e.cwd,
                    "provider": clean(&e.provider),
                    "model": clean(&e.model),
                    "created_at": e.created_at,
                    "last_active_at": e.last_active_at,
                    "records_path": e.records_path,
                })
            })
            .collect(),
    )
}

/// `s`, single-quoted for a POSIX shell when it needs quoting.
pub(crate) fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn entry(id: &str, cwd: Option<&str>, title: &str) -> SessionEntry {
        SessionEntry {
            id: id.to_string(),
            display_title: title.to_string(),
            records_path: PathBuf::from(format!("/r/{id}.jsonl")),
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            provider: "kimi".to_string(),
            model: "k3".to_string(),
            cwd: cwd.map(PathBuf::from),
        }
    }

    #[test]
    fn listing_is_scoped_to_a_directory_unless_all() {
        let entries = vec![
            entry("a", Some("/nonexistent/repo"), "one"),
            entry("b", Some("/nonexistent/other"), "two"),
            entry("c", None, "old"),
        ];
        let repo = PathBuf::from("/nonexistent/repo");
        let ids: Vec<String> = select(entries.clone(), Some(&repo))
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, ["a"]);
        assert_eq!(select(entries, None).len(), 3);
    }

    /// A title cannot break the table or smuggle a terminal escape.
    #[test]
    fn rows_are_aligned_and_cleaned() {
        let rows = vec![
            entry("short", None, "line one\nline two"),
            entry("a-longer-id", None, "\u{1b}[31mred\u{1b}[0m"),
        ];
        let lines = table(&rows);
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("ID           LAST ACTIVE"),
            "{:?}",
            lines[0]
        );
        assert!(lines[1].ends_with("line one line two"), "{:?}", lines[1]);
        assert!(!lines[2].contains('\u{1b}'), "{:?}", lines[2]);
        let model_column = lines[0].find("MODEL").unwrap();
        assert_eq!(lines[1].find("kimi/k3"), Some(model_column));
        assert_eq!(lines[2].find("kimi/k3"), Some(model_column));
    }

    #[test]
    fn json_rows_carry_every_field() {
        let rows = json_rows(&[entry("a", Some("/repo"), "t")]);
        let row = &rows[0];
        assert_eq!(row["id"], "a");
        assert_eq!(row["cwd"], "/repo");
        assert_eq!(row["model"], "k3");
        assert!(row["last_active_at"].is_string());
    }

    #[test]
    fn resume_commands_are_quoted_for_the_shell() {
        assert_eq!(shell_quote("work-2"), "work-2");
        assert_eq!(shell_quote("/home/me/My Repo"), "'/home/me/My Repo'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
