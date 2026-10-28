//! `select_tools` — progressive tool disclosure primitive.
//!
//! Lets the model load dynamic (e.g. MCP) tool definitions by exact name. The
//! loaded names are tracked in `ToolDisclosureState`; the agent loop includes
//! those definitions in the tool table offered on subsequent turns.

use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use lm_bridge::tool::Tool;
use serde_json::json;

use crate::{
    agent_loop::types::{
        ExecutableTool, ExecutableToolContext, ExecutableToolResult, ForkBinding, ToolAccesses,
    },
    tool_disclosure::{LOADED_PREFIX, LOADED_SEPARATOR, ToolDisclosureState},
    tools::ToolRegistry,
};

pub const SELECT_TOOLS_TOOL_NAME: &str = "select_tools";

/// After this many consecutive `select_tools` calls that made no tool
/// available, the result adds the cross-call "stop guessing" warning.
const CONSECUTIVE_FAILURE_THRESHOLD: usize = 3;

pub struct SelectToolsTool {
    tool: Tool,
    /// Weak reference to avoid a reference cycle: the registry owns this tool.
    registry: Weak<Mutex<ToolRegistry>>,
    disclosure: Arc<Mutex<ToolDisclosureState>>,
}

impl SelectToolsTool {
    pub fn new(
        registry: Weak<Mutex<ToolRegistry>>,
        disclosure: Arc<Mutex<ToolDisclosureState>>,
    ) -> Self {
        Self {
            tool: Tool::new(
                SELECT_TOOLS_TOOL_NAME,
                "Load one or more tools by name so you can call them. \
                 All available tool names are listed in the <tools_added>/<tools_removed> announcements \
                 in the system context — fold them in order to get the current list. \
                 If you have not seen a <tools_added> announcement in this session, no tools are \
                 currently loadable — do not call select_tools. \
                 Pass the exact name(s) you need; their full definitions become available immediately, \
                 so you can call them directly in your next tool call.",
                json!({
                    "type": "object",
                    "properties": {
                        "names": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1,
                            "description": "Exact tool names to load, taken from the latest announced tool list."
                        }
                    },
                    "required": ["names"],
                    "additionalProperties": false
                }),
            ),
            registry,
            disclosure,
        }
    }

    /// The requested names, in the order given, without repeats. The order
    /// matters: under a tool cap the first names are the ones loaded.
    fn parse_names(input: &serde_json::Value) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        input
            .get("names")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .filter(|name| seen.insert(name.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Up to three of `candidates` closest to `name`, nearest first. A name is
/// compared both whole and by its last `__` segment, so `search_doc` finds
/// `mcp__zai_zread__search_doc`. Only near misses count: a third of the
/// name's length, and at least two edits.
fn suggestions(name: &str, candidates: &[String]) -> Vec<String> {
    let wanted = name.to_lowercase();
    let limit = (wanted.chars().count() / 3).max(2);
    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .filter_map(|candidate| {
            let lower = candidate.to_lowercase();
            let whole = strsim::levenshtein(&wanted, &lower);
            let tail = lower
                .rsplit("__")
                .next()
                .map_or(whole, |tail| strsim::levenshtein(&wanted, tail));
            let distance = whole.min(tail);
            (distance <= limit).then_some((distance, candidate))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored
        .into_iter()
        .take(3)
        .map(|(_, candidate)| candidate.clone())
        .collect()
}

#[async_trait]
impl ExecutableTool for SelectToolsTool {
    fn tool_definition(&self) -> &Tool {
        &self.tool
    }

    /// A fork loads tools into its own registry and disclosure state.
    fn rebind_for_fork(&self, binding: &ForkBinding) -> Option<Arc<dyn ExecutableTool>> {
        Some(Arc::new(Self::new(
            binding.registry.clone(),
            Arc::clone(&binding.disclosure),
        )))
    }

    fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
        // Serialized against every other tool in the batch to prevent concurrent
        // select_tools calls from double-loading the same schemas.
        ToolAccesses::all()
    }

    fn approval_rule(&self, _input: &serde_json::Value) -> &str {
        "silent"
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: &ExecutableToolContext,
        _tool_call_id: &str,
    ) -> ExecutableToolResult {
        let names = Self::parse_names(&input);
        if names.is_empty() {
            return ExecutableToolResult::error(
                "Invalid input: 'names' must be a non-empty array of tool names.",
            );
        }

        let Some(registry_arc) = self.registry.upgrade() else {
            return ExecutableToolResult::error("Tool registry is no longer available.");
        };
        // How many tools the next request would declare, against the model's
        // cap: taken before the locks below, which `offered_tools` also takes.
        let cap = ctx.llm.as_ref().and_then(|llm| llm.max_tools());
        let in_use = cap.map(|_| {
            crate::agent_loop::run_turn::offered_tools(&registry_arc, ctx, &self.disclosure).len()
        });
        let registry = registry_arc.lock().unwrap();
        // Which of the names the goal state lets through, decided before the
        // disclosure lock: `offered_tools` takes these two locks in that order.
        let goal_offers: std::collections::HashSet<String> = {
            let goal_manager = ctx.goal_manager.lock().unwrap();
            names
                .iter()
                .filter(|name| goal_manager.should_offer_tool(name))
                .cloned()
                .collect()
        };
        let mut disclosure = self.disclosure.lock().unwrap();
        // A registry change resets the failure counters, here and at the
        // announcement boundary in `run_turn`, so they always describe the
        // registry the current names are selected against.
        let fingerprint = ToolDisclosureState::selection_fingerprint(&registry);
        disclosure.reset_failures_if_selection_changed(&fingerprint);

        let mut to_load = Vec::new();
        let mut already_available = Vec::new();
        let mut static_available = Vec::new();
        let mut goal_scoped = Vec::new();
        let mut unavailable = Vec::new();
        let mut stale_loaded = Vec::new();
        let mut unknown = Vec::new();

        for name in names {
            let tool = registry.get(&name);
            let registered = tool.is_some();
            if tool.is_some_and(|tool| !tool.is_available(ctx)) {
                unavailable.push(name);
            } else if registered && registry.is_dynamic(&name) {
                if disclosure.loaded().contains(&name) {
                    already_available.push(name);
                } else {
                    to_load.push(name);
                }
            } else if registered && name != SELECT_TOOLS_TOOL_NAME {
                if goal_offers.contains(&name) {
                    static_available.push(name);
                } else {
                    goal_scoped.push(name);
                }
            } else if disclosure.loaded().contains(&name) {
                // Loaded earlier, but no longer registered (stale-loaded):
                // select_tools cannot bring it back.
                stale_loaded.push(name);
            } else {
                unknown.push(name);
            }
        }

        // Load what fits under the cap, in the order asked; refuse the rest.
        let mut over_cap = Vec::new();
        if let (Some(cap), Some(in_use)) = (cap, in_use) {
            let room = cap.saturating_sub(in_use);
            if to_load.len() > room {
                over_cap = to_load.split_off(room);
            }
        }
        if !to_load.is_empty() {
            disclosure.load(to_load.clone());
        }

        // Track failures per name and across calls. Names that became (or stay)
        // available clear their own record and the consecutive streak; stale or
        // unknown names accumulate attempts toward the escalation texts below.
        // An error only when the call leaves nothing new to call.
        let is_error =
            to_load.is_empty() && already_available.is_empty() && static_available.is_empty();
        let mut succeeded = to_load.clone();
        succeeded.extend(already_available.iter().cloned());
        succeeded.extend(static_available.iter().cloned());
        let mut failed = stale_loaded.clone();
        failed.extend(unknown.iter().cloned());
        let escalated = disclosure.record_selection_outcome(&succeeded, &failed);
        let consecutive_failures = disclosure.consecutive_failures();

        let mut lines = Vec::new();
        if !to_load.is_empty() {
            // A resumed session reads this line back to know what is loaded.
            lines.push(format!("{LOADED_PREFIX}{}", to_load.join(LOADED_SEPARATOR)));
        }
        if !already_available.is_empty() {
            lines.push(format!(
                "Already available: {}",
                already_available.join(", ")
            ));
        }
        for name in &static_available {
            lines.push(format!(
                "{name} is always available — call it directly, without select_tools."
            ));
        }
        for name in &goal_scoped {
            let when = if name == "create_goal" {
                "no goal is active"
            } else {
                "a goal is active"
            };
            lines.push(format!("{name} is available only while {when}."));
        }
        for name in &unavailable {
            lines.push(format!(
                "{name} cannot work in this session, so it is not offered; do without it."
            ));
        }
        for name in &stale_loaded {
            lines.push(format!(
                "\"{name}\" was previously selected or loaded but is no longer registered. \
                 select_tools cannot restore it. Do not select it again unless a new \
                 <tools_added> announcement lists it."
            ));
        }
        if !over_cap.is_empty() {
            lines.push(format!(
                "Not loaded: {}. This model takes at most {} tools per request, and {} are \
                 declared already. Load fewer tools, or work with the ones loaded.",
                over_cap.join(", "),
                cap.unwrap_or_default(),
                in_use.unwrap_or_default()
            ));
        }
        if !unknown.is_empty() {
            let dynamic: Vec<String> = registry
                .dynamic_definition_names()
                .into_iter()
                .map(ToString::to_string)
                .collect();
            for name in unknown {
                let near = suggestions(&name, &dynamic);
                if near.is_empty() {
                    lines.push(format!(
                        "Unknown tool: {name}. Pick from the latest announced tools list."
                    ));
                } else {
                    lines.push(format!(
                        "Unknown tool: {name}. Did you mean: {}?",
                        near.join(", ")
                    ));
                }
            }
        }
        // The categories above stay visible; these lines add the count-based
        // warnings on top.
        for (name, attempts) in &escalated {
            lines.push(format!(
                "\"{name}\" has already failed selection {attempts} times. \
                 Stop retrying this name until its availability changes."
            ));
        }
        if consecutive_failures >= CONSECUTIVE_FAILURE_THRESHOLD {
            lines.push(format!(
                "{consecutive_failures} consecutive select_tools calls failed without making any \
                 tool available. Stop guessing names or retrying unavailable tools. Use only \
                 exact names from the current announcements, use tools already available, or \
                 tell the user what capability is missing."
            ));
        }

        ExecutableToolResult {
            output: vec![lm_bridge::message::ContentPart::Text {
                text: lines.join("\n"),
            }],
            is_error,
            stop_turn: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::types::ExecutableTool;

    struct DummyDynamicTool {
        def: Tool,
    }

    #[async_trait]
    impl ExecutableTool for DummyDynamicTool {
        fn tool_definition(&self) -> &Tool {
            &self.def
        }

        fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
            ToolAccesses::none()
        }

        fn approval_rule(&self, _input: &serde_json::Value) -> &str {
            "silent"
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ExecutableToolContext,
            _tool_call_id: &str,
        ) -> ExecutableToolResult {
            ExecutableToolResult::text("")
        }
    }

    fn ctx() -> ExecutableToolContext {
        use std::collections::HashSet;
        use std::sync::atomic::AtomicBool;
        let background = Arc::new(crate::background::BackgroundManager::new());
        ExecutableToolContext {
            depth: 0,
            max_subagent_depth: crate::agent_loop::types::DEFAULT_MAX_SUBAGENT_DEPTH,
            cwd: std::env::current_dir().unwrap(),
            allowed_paths: HashSet::new(),
            todo_list: crate::TodoList::shared(),
            tool_event_sender: None,
            user_interaction: None,
            search_url: None,
            plan_mode: Arc::new(AtomicBool::new(false)),
            permission_mode: crate::permissions::PermissionMode::Default,
            llm: None,
            vision_fallback: None,
            web_search_client: None,
            web_reader_client: None,
            background: background.clone(),
            cron: Arc::new(crate::cron::CronManager::new()),
            goal_manager: Arc::new(Mutex::new(crate::goal::GoalManager::new())),
            tool_disclosure: None,
            stale_guard: Default::default(),
            fork_source: Default::default(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_select_tools_loads_dynamic_names() {
        let mut registry = ToolRegistry::new();
        registry.register_dynamic(Arc::new(DummyDynamicTool {
            def: Tool::new(
                "dynamic_a",
                "",
                serde_json::Value::Object(Default::default()),
            ),
        }));
        let registry = Arc::new(Mutex::new(registry));
        let disclosure = Arc::new(Mutex::new(ToolDisclosureState::new()));
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure.clone());

        let result = tool
            .execute(json!({ "names": ["dynamic_a", "unknown"] }), &ctx(), "tc")
            .await;

        assert!(!result.is_error);
        let text = result.text_content();
        assert!(text.contains("Loaded: dynamic_a"));
        assert!(text.contains("Unknown tool: unknown"));
        assert!(disclosure.lock().unwrap().loaded().contains("dynamic_a"));
    }

    #[tokio::test]
    async fn test_select_tools_reports_already_loaded() {
        let mut registry = ToolRegistry::new();
        registry.register_dynamic(Arc::new(DummyDynamicTool {
            def: Tool::new(
                "dynamic_b",
                "",
                serde_json::Value::Object(Default::default()),
            ),
        }));
        let registry = Arc::new(Mutex::new(registry));
        let disclosure = Arc::new(Mutex::new(ToolDisclosureState::new()));
        disclosure.lock().unwrap().load(["dynamic_b"]);
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure.clone());

        let result = tool
            .execute(json!({ "names": ["dynamic_b"] }), &ctx(), "tc")
            .await;

        assert!(!result.is_error);
        assert!(result.text_content().contains("Already available"));
    }

    struct StaticTool {
        def: Tool,
    }

    #[async_trait]
    impl ExecutableTool for StaticTool {
        fn tool_definition(&self) -> &Tool {
            &self.def
        }

        fn accesses(&self, _input: &serde_json::Value) -> ToolAccesses {
            ToolAccesses::none()
        }

        fn approval_rule(&self, _input: &serde_json::Value) -> &str {
            "silent"
        }

        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ExecutableToolContext,
            _tool_call_id: &str,
        ) -> ExecutableToolResult {
            ExecutableToolResult::text("")
        }
    }

    fn tool_named(name: &str) -> Tool {
        Tool::new(name, "", serde_json::Value::Object(Default::default()))
    }

    /// A model capped at `cap` tools per request.
    struct CappedLlm {
        cap: usize,
    }

    #[async_trait]
    impl crate::agent_loop::Llm for CappedLlm {
        fn model_name(&self) -> &str {
            "capped"
        }

        fn max_tools(&self) -> Option<usize> {
            Some(self.cap)
        }

        async fn chat(
            &self,
            _messages: Vec<lm_bridge::message::Message>,
            _tools: Vec<Tool>,
            _callbacks: crate::agent_loop::LlmCallbacks,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> Result<crate::agent_loop::types::StepResult, lm_bridge::errors::LlmError> {
            Err(lm_bridge::errors::LlmError::Provider(
                "not called in these tests".to_string(),
            ))
        }
    }

    fn registry_with(
        statics: &[&str],
        dynamics: &[&str],
    ) -> (Arc<Mutex<ToolRegistry>>, Arc<Mutex<ToolDisclosureState>>) {
        let mut registry = ToolRegistry::new();
        for name in statics {
            registry.register(Arc::new(StaticTool {
                def: tool_named(name),
            }));
        }
        for name in dynamics {
            registry.register_dynamic(Arc::new(DummyDynamicTool {
                def: tool_named(name),
            }));
        }
        let registry = Arc::new(Mutex::new(registry));
        let disclosure = Arc::new(Mutex::new(ToolDisclosureState::new()));
        (registry, disclosure)
    }

    /// Asking for a tool that is always in the table points at calling it
    /// directly, and is not an error.
    #[tokio::test]
    async fn a_static_tool_is_already_callable() {
        let (registry, disclosure) = registry_with(&["read_file"], &["mcp__x__lookup"]);
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure);

        let result = tool
            .execute(json!({ "names": ["read_file"] }), &ctx(), "tc")
            .await;

        assert!(!result.is_error, "{}", result.text_content());
        assert!(
            result
                .text_content()
                .contains("read_file is always available — call it directly"),
            "{}",
            result.text_content()
        );
    }

    /// An unknown name gets the nearest announced names, matched whole or by
    /// the last `__` segment of an MCP tool.
    #[tokio::test]
    async fn an_unknown_name_gets_near_misses() {
        let (registry, disclosure) = registry_with(
            &[],
            &[
                "mcp__zai_zread__search_doc",
                "mcp__zai_zread__read_file",
                "mcp__github__create_issue",
            ],
        );
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure);

        let result = tool
            .execute(
                json!({ "names": ["search_docs", "teleport"] }),
                &ctx(),
                "tc",
            )
            .await;

        let text = result.text_content();
        assert!(result.is_error, "nothing was loaded: {text}");
        assert!(
            text.contains("Unknown tool: search_docs. Did you mean: mcp__zai_zread__search_doc?"),
            "{text}"
        );
        assert!(
            text.contains("Unknown tool: teleport. Pick from the latest announced tools list."),
            "{text}"
        );
    }

    #[test]
    fn suggestions_rank_by_distance_and_stop_at_three() {
        let candidates: Vec<String> = [
            "mcp__a__fetch",
            "mcp__b__fetch",
            "mcp__c__fetch",
            "mcp__d__fetch",
            "fetcher",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let near = suggestions("fetch", &candidates);
        assert_eq!(near, ["mcp__a__fetch", "mcp__b__fetch", "mcp__c__fetch"]);
        assert!(suggestions("zzzzzz", &candidates).is_empty());
    }

    /// Under a tool cap, the names that fit are loaded in the order asked, and
    /// the rest are refused with the reason.
    #[tokio::test]
    async fn loading_stops_at_the_models_tool_cap() {
        let (registry, disclosure) = registry_with(&["read_file"], &["dyn_a", "dyn_b", "dyn_c"]);
        {
            let mut reg = registry.lock().unwrap();
            let select = SelectToolsTool::new(Arc::downgrade(&registry), disclosure.clone());
            reg.register(Arc::new(select));
        }
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure.clone());
        let mut context = ctx();
        // read_file + select_tools are declared already; two more fit.
        context.llm = Some(Arc::new(CappedLlm { cap: 4 }));

        let result = tool
            .execute(
                json!({ "names": ["dyn_c", "dyn_a", "dyn_b"] }),
                &context,
                "tc",
            )
            .await;

        let text = result.text_content();
        assert!(!result.is_error, "{text}");
        assert!(text.contains("Loaded: dyn_c, dyn_a"), "{text}");
        assert!(
            text.contains("Not loaded: dyn_b. This model takes at most 4 tools per request, and 2 are declared already."),
            "{text}"
        );
        let loaded = disclosure.lock().unwrap().loaded().clone();
        assert_eq!(loaded.into_iter().collect::<Vec<_>>(), ["dyn_a", "dyn_c"]);

        // The step's tool table honours the same cap.
        let table = crate::agent_loop::run_turn::step_tool_table(&registry, &context, &disclosure);
        assert_eq!(table.len(), 4);
    }

    #[tokio::test]
    async fn a_loaded_tool_that_left_the_registry_is_stale() {
        let registry = Arc::new(Mutex::new(ToolRegistry::new()));
        let disclosure = Arc::new(Mutex::new(ToolDisclosureState::new()));
        disclosure.lock().unwrap().load(["dynamic_removed"]);
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure.clone());

        let result = tool
            .execute(json!({ "names": ["dynamic_removed"] }), &ctx(), "tc")
            .await;

        assert!(result.is_error);
        let text = result.text_content();
        assert!(
            text.contains(
                "\"dynamic_removed\" was previously selected or loaded but is no longer registered."
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "Do not select it again unless a new <tools_added> announcement lists it."
            ),
            "{text}"
        );
        assert!(!text.contains("Unknown tool: dynamic_removed"), "{text}");
    }

    /// A name that fails selection twice is told to stop retrying, with the
    /// attempt count; the first failure only reports the category.
    #[tokio::test]
    async fn a_name_that_fails_twice_is_escalated() {
        let (registry, disclosure) = registry_with(&[], &[]);
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure);

        let first = tool
            .execute(json!({ "names": ["ghost"] }), &ctx(), "tc")
            .await;
        assert!(
            !first
                .text_content()
                .contains("has already failed selection"),
            "{}",
            first.text_content()
        );

        let second = tool
            .execute(json!({ "names": ["ghost"] }), &ctx(), "tc")
            .await;
        let text = second.text_content();
        assert!(
            text.contains(
                "\"ghost\" has already failed selection 2 times. \
                 Stop retrying this name until its availability changes."
            ),
            "{text}"
        );

        let third = tool
            .execute(json!({ "names": ["ghost"] }), &ctx(), "tc")
            .await;
        assert!(
            third
                .text_content()
                .contains("\"ghost\" has already failed selection 3 times."),
            "{}",
            third.text_content()
        );
    }

    /// The cross-call warning lands after three consecutive calls that made
    /// no tool available.
    #[tokio::test]
    async fn three_consecutive_failures_warn_stop_guessing() {
        let (registry, disclosure) = registry_with(&[], &[]);
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure);

        for guess in ["first_guess", "second_guess"] {
            let result = tool
                .execute(json!({ "names": [guess] }), &ctx(), "tc")
                .await;
            assert!(
                !result
                    .text_content()
                    .contains("consecutive select_tools calls"),
                "{}",
                result.text_content()
            );
        }

        let third = tool
            .execute(json!({ "names": ["third_guess"] }), &ctx(), "tc")
            .await;
        let text = third.text_content();
        assert!(
            text.contains(
                "3 consecutive select_tools calls failed without making any tool available. \
                 Stop guessing names or retrying unavailable tools."
            ),
            "{text}"
        );
        // The per-name reasons stay visible next to the warning.
        assert!(text.contains("Unknown tool: third_guess."), "{text}");
    }

    /// A success in the batch resets the consecutive-failure streak and the
    /// successful name's own record, but a failed sibling keeps accumulating
    /// attempts.
    #[tokio::test]
    async fn a_sibling_success_resets_the_streak_but_not_sibling_failures() {
        let (registry, disclosure) = registry_with(&[], &["dyn_a"]);
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure);

        let mixed = tool
            .execute(json!({ "names": ["dyn_a", "ghost"] }), &ctx(), "tc")
            .await;
        assert!(!mixed.is_error, "{}", mixed.text_content());
        assert!(
            !mixed
                .text_content()
                .contains("has already failed selection"),
            "{}",
            mixed.text_content()
        );

        let failed = tool
            .execute(json!({ "names": ["ghost"] }), &ctx(), "tc")
            .await;
        let text = failed.text_content();
        assert!(
            text.contains("\"ghost\" has already failed selection 2 times."),
            "{text}"
        );
        // One failed call after the success: no cross-call warning yet.
        assert!(!text.contains("consecutive select_tools calls"), "{text}");

        // The successful name's record was cleared: asking for it again is
        // not an escalation.
        let again = tool
            .execute(json!({ "names": ["dyn_a"] }), &ctx(), "tc")
            .await;
        assert!(
            again.text_content().contains("Already available: dyn_a"),
            "{}",
            again.text_content()
        );
        assert!(
            !again
                .text_content()
                .contains("has already failed selection"),
            "{}",
            again.text_content()
        );
    }

    /// Failure records are keyed case-insensitively: selecting `Echo` clears
    /// a record left by `echo`.
    #[tokio::test]
    async fn a_success_clears_the_failure_record_case_insensitively() {
        let (registry, disclosure) = registry_with(&["Echo"], &[]);
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure);

        tool.execute(json!({ "names": ["echo", "ghost"] }), &ctx(), "tc")
            .await;
        let after = tool
            .execute(json!({ "names": ["echo", "ghost"] }), &ctx(), "tc")
            .await;
        assert!(
            after
                .text_content()
                .contains("\"ghost\" has already failed selection 2 times."),
            "{}",
            after.text_content()
        );

        tool.execute(json!({ "names": ["Echo"] }), &ctx(), "tc")
            .await;
        let next = tool
            .execute(json!({ "names": ["ghost"] }), &ctx(), "tc")
            .await;
        let text = next.text_content();
        assert!(
            text.contains("\"ghost\" has already failed selection 3 times."),
            "{text}"
        );
        assert!(!text.contains("\"echo\" has already failed"), "{text}");
    }

    /// Both counters reset when the registry changes: a tool vanishing or
    /// appearing makes the old failure counts moot.
    #[tokio::test]
    async fn failure_counters_reset_when_the_registry_changes() {
        let (registry, disclosure) = registry_with(&[], &["dyn_a"]);
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure.clone());

        tool.execute(json!({ "names": ["ghost"] }), &ctx(), "tc")
            .await;
        let second = tool
            .execute(json!({ "names": ["ghost"] }), &ctx(), "tc")
            .await;
        assert!(
            second
                .text_content()
                .contains("has already failed selection 2 times"),
            "{}",
            second.text_content()
        );

        // The registry changes: a stale-loaded name appears. The next failure
        // starts from a clean slate.
        registry.lock().unwrap().unregister("dyn_a");
        let third = tool
            .execute(json!({ "names": ["ghost"] }), &ctx(), "tc")
            .await;
        let text = third.text_content();
        assert!(!text.contains("has already failed selection"), "{text}");
        assert!(!text.contains("consecutive select_tools calls"), "{text}");
    }

    /// A stale-loaded name counts toward both the per-name and the
    /// consecutive failures, like an unknown one.
    #[tokio::test]
    async fn stale_loaded_names_count_as_failures() {
        let registry = Arc::new(Mutex::new(ToolRegistry::new()));
        let disclosure = Arc::new(Mutex::new(ToolDisclosureState::new()));
        disclosure.lock().unwrap().load(["gone_a", "gone_b"]);
        let tool = SelectToolsTool::new(Arc::downgrade(&registry), disclosure);

        tool.execute(json!({ "names": ["gone_a"] }), &ctx(), "tc")
            .await;
        tool.execute(json!({ "names": ["gone_b"] }), &ctx(), "tc")
            .await;
        let third = tool
            .execute(json!({ "names": ["gone_a"] }), &ctx(), "tc")
            .await;
        let text = third.text_content();
        assert!(
            text.contains("\"gone_a\" has already failed selection 2 times."),
            "{text}"
        );
        assert!(
            text.contains("3 consecutive select_tools calls failed"),
            "{text}"
        );
    }
}
