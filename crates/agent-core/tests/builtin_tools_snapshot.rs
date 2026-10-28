//! Snapshot of the built-in tool set registered by `Agent::new`.
//!
//! `docs/tools.md` documents these names. If a tool is added, removed, or
//! renamed, this test fails and the docs must be updated in the same change.

use std::path::PathBuf;
use std::sync::Arc;

use agent_core::Agent;
use agent_core::vision_fallback::VisionFallback;

/// Built-in tools advertised by a fresh agent with no active goal, once it
/// has a search service for `web_search` and a way to see media for
/// `read_media`; without them, those two are not offered.
///
/// Two further tools — `update_goal` and `set_goal_budget` — are registered by
/// `Agent::new` but hidden by `list_tools` until a goal is active, so the
/// registry holds 28 while only these 26 are advertised.
///
/// `web_reader` and `skill` are absent entirely: they are registered later, and
/// only when a web-reader client or a skill registry is attached.
const VISIBLE_WITHOUT_ACTIVE_GOAL: &[&str] = &[
    "agent",
    "agent_swarm",
    "ask_user",
    "ask_user_question",
    "bash",
    "create_goal",
    "cron_create",
    "cron_delete",
    "cron_list",
    "edit_file",
    "exit_plan_mode",
    "fetch",
    "get_goal",
    "glob",
    "grep",
    "list_dir",
    "read_file",
    "read_media",
    "select_tools",
    "task_list",
    "task_output",
    "task_stop",
    "todo_list",
    "wait_for",
    "web_search",
    "write_file",
];

/// Registered by `Agent::new`, but only advertised once a goal is active.
const HIDDEN_UNTIL_GOAL_ACTIVE: &[&str] = &["set_goal_budget", "update_goal"];

/// Offered only with what they need, which a bare agent lacks.
const NEED_A_SERVICE: &[&str] = &["read_media", "web_search"];

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// A vision fallback that never needs to answer: `read_media` is offered
/// once one is attached.
struct Describer;

#[async_trait::async_trait]
impl VisionFallback for Describer {
    async fn describe_image(
        &self,
        _mime: &str,
        _base64_data: &str,
        _question: Option<&str>,
    ) -> Result<String, String> {
        Err("not asked in this test".to_string())
    }
}

/// An agent with all the built-in tools available.
fn equipped_agent() -> Agent {
    Agent::new(PathBuf::from("."), "test")
        .with_search_url("https://search.example/?q={query}")
        .with_vision_fallback(Arc::new(Describer))
}

#[test]
fn visible_builtin_tool_set_matches_readme() {
    let actual = sorted(equipped_agent().list_tools());

    let expected: Vec<String> = sorted(
        VISIBLE_WITHOUT_ACTIVE_GOAL
            .iter()
            .map(|s| s.to_string())
            .collect(),
    );

    assert_eq!(
        actual, expected,
        "built-in tool set changed; update docs/tools.md and this snapshot together"
    );
}

#[test]
fn goal_scoped_tools_are_hidden_without_an_active_goal() {
    let agent = Agent::new(PathBuf::from("."), "test");
    let visible = agent.list_tools();

    for name in HIDDEN_UNTIL_GOAL_ACTIVE {
        assert!(
            !visible.contains(&name.to_string()),
            "`{name}` must stay hidden until a goal is active"
        );
    }
}

#[test]
fn visible_builtin_tool_count_is_stable() {
    assert_eq!(
        equipped_agent().list_tools().len(),
        VISIBLE_WITHOUT_ACTIVE_GOAL.len(),
        "built-in tool count changed"
    );
}

/// Without a search service or a way to see media, `web_search` and
/// `read_media` are left out, and only they.
#[test]
fn tools_that_cannot_work_are_not_listed() {
    let actual = sorted(Agent::new(PathBuf::from("."), "test").list_tools());
    let expected: Vec<String> = VISIBLE_WITHOUT_ACTIVE_GOAL
        .iter()
        .filter(|name| !NEED_A_SERVICE.contains(name))
        .map(|name| name.to_string())
        .collect();
    assert_eq!(actual, expected);
}
