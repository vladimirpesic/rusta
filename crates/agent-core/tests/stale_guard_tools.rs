//! End-to-end behaviour of the staleness guard across `read_file`,
//! `edit_file` and `write_file`: `write_file` replaces only a file the agent
//! has read and that has not changed since, while `edit_file`, whose
//! `old_string` must match the file as it is, needs no read.
//!
//! The unit tests in `agent_core::stale_guard` cover the bookkeeping. These
//! drive the real tools against a real filesystem, which is the only way to
//! check that the stamps the tools take actually line up with each other — a
//! guard whose reader and writer disagree about what a file looks like would
//! refuse every write, and no amount of unit testing the map would show it.

use std::sync::Arc;

use agent_core::agent_loop::types::{ExecutableTool, ExecutableToolContext, ExecutableToolResult};
use agent_core::tools::builtin::{EditFileTool, ReadFileTool, WriteFileTool};
use serde_json::json;

fn ctx(cwd: &std::path::Path) -> ExecutableToolContext {
    let background = Arc::new(agent_core::background::BackgroundManager::new());
    ExecutableToolContext {
        depth: 0,
        max_subagent_depth: agent_core::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
        cwd: cwd.to_path_buf(),
        allowed_paths: std::iter::once(cwd.to_path_buf()).collect(),
        todo_list: agent_core::TodoList::shared(),
        tool_event_sender: None,
        user_interaction: None,
        search_url: None,
        plan_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        permission_mode: agent_core::permissions::PermissionMode::Default,
        llm: None,
        vision_fallback: None,
        web_search_client: None,
        web_reader_client: None,
        background: background.clone(),
        cron: Arc::new(agent_core::cron::CronManager::new()),
        goal_manager: Arc::new(std::sync::Mutex::new(agent_core::goal::GoalManager::new())),
        tool_disclosure: None,
        stale_guard: Default::default(),
        fork_source: Default::default(),
        ..Default::default()
    }
}

async fn read(path: &std::path::Path, ctx: &ExecutableToolContext) -> ExecutableToolResult {
    ReadFileTool::new()
        .execute(json!({"path": path.to_string_lossy()}), ctx, "r")
        .await
}

async fn edit(
    path: &std::path::Path,
    old: &str,
    new: &str,
    ctx: &ExecutableToolContext,
) -> ExecutableToolResult {
    EditFileTool::new()
        .execute(
            json!({"path": path.to_string_lossy(), "old_string": old, "new_string": new}),
            ctx,
            "e",
        )
        .await
}

async fn write(
    path: &std::path::Path,
    content: &str,
    ctx: &ExecutableToolContext,
) -> ExecutableToolResult {
    WriteFileTool::new()
        .execute(
            json!({"path": path.to_string_lossy(), "content": content}),
            ctx,
            "w",
        )
        .await
}

/// An edit needs no read: its `old_string` must match the file as it is, so
/// the edit itself shows the model knows the text it changes. Coder sub-agents
/// that read a file with `cat` had every such edit refused, one round trip
/// each. The edit does not count as a read: the file still cannot be
/// overwritten unread.
#[tokio::test]
async fn an_unread_file_may_be_edited_but_not_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "hello world").unwrap();

    let ctx = ctx(dir.path());
    let result = edit(&path, "hello", "goodbye", &ctx).await;
    assert!(!result.is_error, "{}", result.text_content());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");

    let result = write(&path, "clobbered", &ctx).await;
    assert!(result.is_error, "an edit is no read of the whole file");
    assert!(
        result.text_content().contains("has not been read"),
        "the refusal must say why: {}",
        result.text_content()
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
}

/// ...and reading it first is all it takes.
#[tokio::test]
async fn reading_first_permits_the_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "hello world").unwrap();

    let ctx = ctx(dir.path());
    assert!(!read(&path, &ctx).await.is_error);
    let result = edit(&path, "hello", "goodbye", &ctx).await;

    assert!(!result.is_error, "{}", result.text_content());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "goodbye world");
}

/// A change between the read and an overwrite is the other half of the guard:
/// the model's picture is out of date even though it did read the file.
#[tokio::test]
async fn a_change_after_the_read_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "hello world").unwrap();

    let ctx = ctx(dir.path());
    assert!(!read(&path, &ctx).await.is_error);
    std::fs::write(&path, "hello there, world").unwrap();

    let result = write(&path, "goodbye", &ctx).await;
    assert!(result.is_error, "a changed file must not be overwritten");
    assert!(
        result.text_content().contains("changed on disk"),
        "the refusal must distinguish itself from never-read: {}",
        result.text_content()
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "hello there, world",
        "the other party's change must survive"
    );
}

/// An edit of a file that changed since the read changes only its match, so
/// the other party's change survives; but the agent's picture of the rest is
/// out of date, so the file cannot then be overwritten without a new read.
#[tokio::test]
async fn an_edit_after_a_change_keeps_it_and_licenses_no_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "hello world").unwrap();

    let ctx = ctx(dir.path());
    assert!(!read(&path, &ctx).await.is_error);
    std::fs::write(&path, "hello there, world").unwrap();

    let result = edit(&path, "hello", "goodbye", &ctx).await;
    assert!(!result.is_error, "{}", result.text_content());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "goodbye there, world"
    );
    assert!(write(&path, "goodbye", &ctx).await.is_error);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "goodbye there, world"
    );
}

/// **The change length and mtime both miss.** `"hello world"` and
/// `"hello there"` are both 11 bytes, and a programmatic rewrite lands inside
/// one mtime tick — the shape of a formatter reflowing a line. Only the content
/// digest catches it.
#[tokio::test]
async fn an_equal_length_rewrite_is_still_caught() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "hello world").unwrap();

    let ctx = ctx(dir.path());
    assert!(!read(&path, &ctx).await.is_error);
    std::fs::write(&path, "hello there").unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        11,
        "the premise of this test is that the length did not change"
    );

    let result = write(&path, "goodbye", &ctx).await;
    assert!(result.is_error, "an equal-length rewrite must be refused");
    assert!(result.text_content().contains("changed on disk"));
}

/// Re-reading clears it, which is exactly what the refusal tells the model to do.
#[tokio::test]
async fn re_reading_clears_the_staleness() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "hello world").unwrap();

    let ctx = ctx(dir.path());
    assert!(!read(&path, &ctx).await.is_error);
    std::fs::write(&path, "hello there again").unwrap();
    assert!(write(&path, "goodbye", &ctx).await.is_error);

    assert!(!read(&path, &ctx).await.is_error);
    let result = write(&path, "goodbye", &ctx).await;
    assert!(!result.is_error, "{}", result.text_content());
}

/// **The agent's own writes must not trip its guard.** A run of edits to one
/// file is completely ordinary, and re-stamping after each edit of a file the
/// agent read is what keeps an overwrite after them from being refused as
/// "changed on disk".
#[tokio::test]
async fn consecutive_edits_to_one_file_are_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "one two three").unwrap();

    let ctx = ctx(dir.path());
    assert!(!read(&path, &ctx).await.is_error);
    for (old, new) in [("one", "1"), ("two", "2"), ("three", "3")] {
        let result = edit(&path, old, new, &ctx).await;
        assert!(!result.is_error, "{}: {}", old, result.text_content());
    }
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 2 3");
    let result = write(&path, "one two three", &ctx).await;
    assert!(!result.is_error, "{}", result.text_content());
}

/// Creating a file destroys nothing, so a path that does not exist needs no
/// prior read — demanding one would be a refusal with no remedy.
#[tokio::test]
async fn writing_a_new_file_needs_no_prior_read() {
    let dir = tempfile::tempdir().unwrap();
    // A flat path: `resolve_allowed_path` canonicalizes, so it cannot resolve a
    // path whose parent directory does not exist yet. That is a pre-existing
    // limit of the path sandbox and unrelated to the guard under test here.
    let path = dir.path().join("new.txt");

    let ctx = ctx(dir.path());
    let result = write(&path, "fresh", &ctx).await;
    assert!(!result.is_error, "{}", result.text_content());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
}

/// Overwriting an *existing* file is the dangerous case: a whole-file write
/// replaces everything, from context that may be many turns old.
#[tokio::test]
async fn overwriting_an_unread_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "precious").unwrap();

    let ctx = ctx(dir.path());
    assert!(write(&path, "clobbered", &ctx).await.is_error);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "precious");
}

/// Having written a file, the agent has seen it — so it may write it again
/// without a read in between.
#[tokio::test]
async fn a_file_this_agent_created_may_be_written_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");

    let ctx = ctx(dir.path());
    assert!(!write(&path, "first", &ctx).await.is_error);
    let result = write(&path, "second", &ctx).await;
    assert!(!result.is_error, "{}", result.text_content());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
}

/// **The guard is per-agent.** Two agents do not share a view of the workspace,
/// and one having read a file says nothing about what the other believes is in
/// it — which is the whole reason a sub-agent runs in an isolated context.
#[tokio::test]
async fn one_agents_read_does_not_license_anothers_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, "hello world").unwrap();

    let reader = ctx(dir.path());
    let writer = ctx(dir.path());
    assert!(!read(&path, &reader).await.is_error);

    assert!(
        write(&path, "goodbye", &writer).await.is_error,
        "the second agent has read nothing and must not overwrite"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
}

/// The guard must not fire on paths the agent never touches — it is a check on
/// one file, not a mode the whole session enters.
#[tokio::test]
async fn reading_one_file_does_not_license_writing_another() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, "a").unwrap();
    std::fs::write(&b, "b").unwrap();

    let ctx = ctx(dir.path());
    assert!(!read(&a, &ctx).await.is_error);
    assert!(write(&b, "B", &ctx).await.is_error);
    assert!(!write(&a, "A", &ctx).await.is_error);
}
