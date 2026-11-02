//! Progressive tool disclosure state.
//!
//! Keeps track of which dynamic (loadable) tools have been announced to the
//! model and which ones the model has explicitly loaded via `select_tools`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use lm_bridge::message::{Message, Role};
use regex::Regex;

use crate::tools::ToolRegistry;
use crate::tools::builtin::select_tools::SELECT_TOOLS_TOOL_NAME;

/// Separator between a tool name and its one-line description in announcements.
const NAME_DESC_SEPARATOR: &str = " — ";

/// A name that has failed selection this many times is told to stop retrying
/// it.
pub(crate) const STOP_RETRY_THRESHOLD: usize = 2;

/// Marker sentence of the empty-loadable-set announcement. Folded back from a
/// resumed history to recognize that the empty state was already announced.
pub(crate) const EMPTY_ANNOUNCEMENT_MARKER: &str =
    "No tools are currently available for dynamic loading in this session.";

/// How a `select_tools` answer lists the tools it loaded: this prefix, then the
/// names joined by [`LOADED_SEPARATOR`]. Written by `select_tools` and read back
/// by [`ToolDisclosureState::fold_loaded_from_history`].
pub(crate) const LOADED_PREFIX: &str = "Loaded: ";
/// Separator between the names on a [`LOADED_PREFIX`] line.
pub(crate) const LOADED_SEPARATOR: &str = ", ";

/// Maximum number of characters of a tool description carried in an announcement.
/// Announcements list every loadable tool, so descriptions are trimmed to keep
/// the context cost bounded even with large MCP servers.
const MAX_DESCRIPTION_LEN: usize = 140;

/// State for the progressive disclosure protocol.
#[derive(Debug, Clone, Default)]
pub struct ToolDisclosureState {
    /// Names the model has been told are loadable.
    announced: BTreeSet<String>,
    /// Names the model has explicitly loaded.
    loaded: BTreeSet<String>,
    /// Loaded tools whose definitions are in the context on a system message
    /// — for a model that takes tools mid-conversation (`messages[].tools`).
    delivered: BTreeSet<String>,
    /// Tools the last capped tool table left out, so the warning is given
    /// once per change rather than on every step.
    capped: Vec<String>,
    /// Per-name failed `select_tools` attempts, keyed by lowercased name.
    /// A name is added on every selection that leaves it unavailable
    /// (stale-loaded or unknown) and removed as soon as a selection
    /// succeeds for it; past the stop-retry threshold the model is told to
    /// stop retrying it.
    failed_selection_attempts: BTreeMap<String, usize>,
    /// How many `select_tools` calls in a row made no tool available.
    consecutive_failures: usize,
    /// The registry fingerprint the failure counters were recorded
    /// against; a change resets them (see
    /// [`Self::reset_failures_if_selection_changed`]).
    selection_fingerprint: Option<String>,
    /// Whether the empty-loadable-set announcement went out. It is issued
    /// once per empty stretch and folded back from a resumed history.
    announced_empty: bool,
}

impl ToolDisclosureState {
    /// Create an empty disclosure state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a set of names as loaded. Unknown/stale names are ignored.
    pub fn load<I>(&mut self, names: I)
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        for name in names {
            self.loaded.insert(name.into());
        }
    }

    /// Mark a name as no longer loaded.
    pub fn unload(&mut self, name: &str) {
        self.loaded.remove(name);
        self.delivered.remove(name);
    }

    /// The loaded tools whose definitions have not been put in the context
    /// yet, now counted as delivered. For a model that takes tools on a
    /// system message rather than in the top-level list.
    pub fn take_undelivered(&mut self) -> Vec<String> {
        let fresh: Vec<String> = self.loaded.difference(&self.delivered).cloned().collect();
        self.delivered.extend(fresh.iter().cloned());
        fresh
    }

    /// Record which tools a capped tool table left out. `true` when that
    /// differs from last time, so the caller says so once.
    pub fn note_capped(&mut self, left_out: &[String]) -> bool {
        if self.capped == left_out {
            return false;
        }
        self.capped = left_out.to_vec();
        true
    }

    /// Fingerprint of what the registry offers `select_tools`: every
    /// registered name and whether it is dynamic, independent of the
    /// registration order.
    pub fn selection_fingerprint(registry: &ToolRegistry) -> String {
        let mut entries: Vec<String> = registry
            .definitions()
            .iter()
            .map(|tool| {
                format!(
                    "{}{}",
                    tool.name,
                    if registry.is_dynamic(&tool.name) {
                        "*"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        entries.sort();
        entries.join("\n")
    }

    /// Reset the `select_tools` failure counters when the registry changed
    /// since they were recorded. A tool appearing or vanishing makes past
    /// failures moot, so a name that failed against the old registry does
    /// not stay escalated after its availability changed — and a sibling
    /// success cannot mask failures that persist across registry states.
    /// Cheap to call on every selection and announcement.
    pub fn reset_failures_if_selection_changed(&mut self, fingerprint: &str) {
        if self.selection_fingerprint.as_deref() == Some(fingerprint) {
            return;
        }
        self.selection_fingerprint = Some(fingerprint.to_string());
        self.consecutive_failures = 0;
        self.failed_selection_attempts.clear();
    }

    /// Record a `select_tools` call's outcome. `succeeded` names made (or
    /// keep) a tool available and clear their own failure record — and, if
    /// any succeeded, the consecutive-failure streak — while `failed` names
    /// (stale-loaded or unknown) each add one attempt, case-insensitively,
    /// counting duplicates within the call once. Returns the failed names
    /// that reached the stop-retry threshold, mapped to the count.
    pub fn record_selection_outcome(
        &mut self,
        succeeded: &[String],
        failed: &[String],
    ) -> Vec<(String, usize)> {
        if succeeded.is_empty() {
            self.consecutive_failures += 1;
        } else {
            self.consecutive_failures = 0;
        }
        for name in succeeded {
            self.failed_selection_attempts.remove(&name.to_lowercase());
        }
        let mut escalated = Vec::new();
        let mut attempts_by_key: HashMap<String, usize> = HashMap::new();
        for name in failed {
            let key = name.to_lowercase();
            let attempts = match attempts_by_key.get(&key) {
                Some(&attempts) => attempts,
                None => {
                    let attempts = self
                        .failed_selection_attempts
                        .get(&key)
                        .copied()
                        .unwrap_or(0)
                        + 1;
                    self.failed_selection_attempts.insert(key.clone(), attempts);
                    attempts_by_key.insert(key, attempts);
                    attempts
                }
            };
            if attempts >= STOP_RETRY_THRESHOLD {
                escalated.push((name.clone(), attempts));
            }
        }
        escalated
    }

    /// How many `select_tools` calls in a row made no tool available.
    pub fn consecutive_failures(&self) -> usize {
        self.consecutive_failures
    }

    /// Reconstruct the announced set from prior `<tools_added>` /
    /// `<tools_removed>` system messages already in the context. This prevents
    /// duplicate announcements after a resumed session.
    pub fn fold_announcements_from_history(&mut self, messages: &[Message]) {
        static ADDED_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
            Regex::new(r"<tools_added>\s*([\s\S]*?)\s*</tools_added>").unwrap()
        });
        static REMOVED_RE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
            Regex::new(r"<tools_removed>\s*([\s\S]*?)\s*</tools_removed>").unwrap()
        });

        for message in messages {
            if message.role != Role::System {
                continue;
            }
            // Definitions delivered on a system message were loaded, and are
            // still in the context.
            for tool in &message.tools {
                self.loaded.insert(tool.name.clone());
                self.delivered.insert(tool.name.clone());
            }
            let text = message.text_content();
            if text.contains(EMPTY_ANNOUNCEMENT_MARKER) {
                self.announced_empty = true;
            }
            for caps in ADDED_RE.captures_iter(&text) {
                for line in caps[1].lines() {
                    if let Some(name) = parse_announced_name(line) {
                        self.announced.insert(name.to_string());
                    }
                }
            }
            for caps in REMOVED_RE.captures_iter(&text) {
                for line in caps[1].lines() {
                    if let Some(name) = parse_announced_name(line) {
                        self.announced.remove(name);
                    }
                }
            }
        }
    }

    /// Reconstruct the loaded set from the `select_tools` exchanges in
    /// `messages`: every name an answer lists on its `Loaded: ` line.
    ///
    /// A model that takes loaded tools in the request's top-level tool list —
    /// every model without `messages[].tools` — has nothing else in the history
    /// that records them; the set lives in memory. A resumed conversation still
    /// shows the model its own `select_tools` calls and their "Loaded: …"
    /// answers, so it calls those tools as loaded while the request no longer
    /// declares them. An endpoint that rejects the undeclared name costs a
    /// failed step; Z.AI constrains the name to a declared tool and runs a
    /// different one.
    ///
    /// `expand` resolves a tool result's text: the records keep a large result
    /// as a blob reference.
    pub fn fold_loaded_from_history(
        &mut self,
        messages: &[Message],
        expand: impl Fn(&str) -> String,
    ) {
        let mut select_calls: HashSet<&str> = HashSet::new();
        for message in messages {
            match message.role {
                Role::Assistant => select_calls.extend(
                    message
                        .tool_calls
                        .iter()
                        .filter(|call| call.name == SELECT_TOOLS_TOOL_NAME)
                        .map(|call| call.id.as_str()),
                ),
                Role::Tool => {
                    let answers_select = message
                        .tool_call_id
                        .as_deref()
                        .is_some_and(|id| select_calls.contains(id));
                    if !answers_select {
                        continue;
                    }
                    let text = expand(&message.text_content());
                    for line in text.lines() {
                        if let Some(names) = line.strip_prefix(LOADED_PREFIX) {
                            self.loaded.extend(
                                names
                                    .split(LOADED_SEPARATOR)
                                    .map(str::trim)
                                    .filter(|name| !name.is_empty())
                                    .map(str::to_string),
                            );
                        }
                    }
                }
                Role::System | Role::User => {}
            }
        }
    }

    /// Return the currently loaded dynamic tool names.
    pub fn loaded(&self) -> &BTreeSet<String> {
        &self.loaded
    }

    /// Forget which tools were announced and which definitions were
    /// delivered, so the next step announces every loadable tool and delivers
    /// the loaded ones' definitions again. Called when the history lost them
    /// (compaction, a clear, an undo); loaded tools stay loaded. The
    /// empty-set announcement is not repeated: it stays silent until a
    /// `<tools_added>` announcement names new tools, and a session resumed
    /// from a history that lost the marker announces it once via the fold
    /// path.
    pub fn forget_announcements(&mut self) {
        self.announced.clear();
        self.delivered.clear();
    }

    /// Compute the diff between the currently available dynamic tools and the
    /// set already announced, return any announcement system messages, and
    /// update the announced set.
    ///
    /// `available` maps each loadable tool name to its one-line description.
    /// Descriptions are included in the announcement so the model can pick the
    /// right tool to `select_tools` without guessing from the name alone.
    ///
    /// When nothing is loadable at all, the empty state is announced once per
    /// stretch — including for a resumed history that lacks its marker — and
    /// then stays silent until a `<tools_added>` announcement names new tools.
    pub fn take_announcement_messages(
        &mut self,
        available: &BTreeMap<String, String>,
    ) -> Vec<Message> {
        let names: BTreeSet<String> = available.keys().cloned().collect();
        let added: Vec<(String, String)> = names
            .difference(&self.announced)
            .map(|name| {
                (
                    name.clone(),
                    available.get(name).cloned().unwrap_or_default(),
                )
            })
            .collect();
        let removed: Vec<String> = self.announced.difference(&names).cloned().collect();
        let has_loadable = !names.is_empty();

        if added.is_empty() && removed.is_empty() {
            if has_loadable || self.announced_empty {
                return Vec::new();
            }
            // Nothing loadable at all: say so once, including for a resumed
            // history that lacks the marker, so `announced_empty` is false.
            self.announced_empty = true;
            return vec![Message::system(render_loadable_tools_announcement(
                &[],
                &[],
                false,
            ))];
        }

        self.announced = names;
        // A rendering with no loadable tools carries the empty-set marker.
        self.announced_empty = !has_loadable;
        let text = render_loadable_tools_announcement(&added, &removed, has_loadable);
        vec![Message::system(text)]
    }
}

/// Extract the MCP server name from a qualified `mcp__<server>__<tool>` name.
fn mcp_server_of(tool_name: &str) -> Option<&str> {
    let rest = tool_name.strip_prefix("mcp__")?;
    let end = rest.find("__")?;
    Some(&rest[..end])
}

/// Collapse a description to a single trimmed line of bounded length.
fn truncate_description(description: &str) -> String {
    let one_line = description
        .split(['\n', '\r'])
        .next()
        .unwrap_or_default()
        .trim();
    if one_line.chars().count() <= MAX_DESCRIPTION_LEN {
        return one_line.to_string();
    }
    let head: String = one_line.chars().take(MAX_DESCRIPTION_LEN).collect();
    format!("{}…", head.trim_end())
}

fn render_tool_line(name: &str, description: &str) -> String {
    let description = truncate_description(description);
    if description.is_empty() {
        name.to_string()
    } else {
        format!("{name}{NAME_DESC_SEPARATOR}{description}")
    }
}

/// Recover a tool name from an announcement line, skipping group headers.
fn parse_announced_name(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    // Tool names never contain whitespace, so the segment before the first
    // separator is the name whether or not a description is present.
    let name = line
        .split(NAME_DESC_SEPARATOR)
        .next()
        .unwrap_or(line)
        .trim();
    (!name.is_empty()).then_some(name)
}

/// Render the added tools, grouping MCP tools under a `## <server>` header so
/// large servers stay navigable. Non-MCP dynamic tools are listed first.
fn render_added_section(added: &[(String, String)]) -> String {
    let mut ungrouped: Vec<String> = Vec::new();
    let mut grouped: BTreeMap<&str, Vec<String>> = BTreeMap::new();

    for (name, description) in added {
        let line = render_tool_line(name, description);
        match mcp_server_of(name) {
            Some(server) => grouped.entry(server).or_default().push(line),
            None => ungrouped.push(line),
        }
    }

    let mut blocks = Vec::new();
    if !ungrouped.is_empty() {
        blocks.push(ungrouped.join("\n"));
    }
    for (server, lines) in grouped {
        blocks.push(format!("## {server}\n{}", lines.join("\n")));
    }
    blocks.join("\n\n")
}

fn render_loadable_tools_announcement(
    added: &[(String, String)],
    removed: &[String],
    has_loadable_tools: bool,
) -> String {
    let mut sections = Vec::new();
    if !added.is_empty() {
        sections.push(format!(
            "<tools_added>\n{}\n</tools_added>",
            render_added_section(added)
        ));
    }
    if !removed.is_empty() {
        sections.push(format!(
            "<tools_removed>\n{}\n</tools_removed>",
            removed.join("\n")
        ));
    }
    if !has_loadable_tools {
        sections.push(
            "No tools are currently available for dynamic loading in this session. \
             Do not call select_tools or guess tool names; use the tools you already have. \
             Only a later <tools_added> announcement can make new tools loadable."
                .to_string(),
        );
        return sections.join("\n\n");
    }
    sections.push(
        "Each line above is `tool_name — description`; `## <server>` headers group tools by MCP server. \
         Use the select_tools tool with the exact tool name (the text before the dash) to load full tool \
         definitions before calling them. Select only the tools you actually need for the current task. \
         Names listed as removed are no longer loadable — do not select them. \
         Fold all announcements in this conversation in order to get the current list."
            .to_string(),
    );
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Loaded tools are handed over once; forgetting (compaction) hands them
    /// over again; a resumed history's delivered tools count as loaded.
    #[test]
    fn loaded_tools_are_delivered_once_until_forgotten() {
        let mut state = ToolDisclosureState::new();
        state.load(["b", "a"]);
        assert_eq!(state.take_undelivered(), ["a", "b"]);
        assert!(state.take_undelivered().is_empty());
        state.forget_announcements();
        assert_eq!(state.take_undelivered(), ["a", "b"]);

        let delivered = Message {
            role: Role::System,
            content: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            tools: vec![lm_bridge::tool::Tool::new(
                "c",
                "",
                serde_json::json!({"type": "object"}),
            )],
            partial: false,
        };
        let mut resumed = ToolDisclosureState::new();
        resumed.fold_announcements_from_history(&[delivered]);
        assert!(resumed.loaded().contains("c"));
        assert!(
            resumed.take_undelivered().is_empty(),
            "already in the history"
        );
    }

    /// A resumed history's `select_tools` answers say what is loaded; a
    /// "Loaded:" line in another tool's result says nothing.
    #[test]
    fn select_tools_answers_in_a_history_load_their_tools() {
        use lm_bridge::message::ToolCall;
        let history = [
            Message::assistant_tool_calls(vec![
                ToolCall::new("call-1", SELECT_TOOLS_TOOL_NAME)
                    .with_arguments(r#"{"names":["b","a","zz"]}"#),
                ToolCall::new("call-2", "bash").with_arguments("{}"),
            ]),
            Message::tool_result(
                "call-1",
                "Loaded: b, a\nUnknown tool: zz. Pick from the latest announced tools list.",
            ),
            Message::tool_result("call-2", "Loaded: not_a_tool"),
            Message::assistant_tool_calls(vec![
                ToolCall::new("call-3", SELECT_TOOLS_TOOL_NAME)
                    .with_arguments(r#"{"names":["a","c"]}"#),
            ]),
            Message::tool_result("call-3", "Loaded: c\nAlready available: a"),
        ];
        let mut state = ToolDisclosureState::new();
        state.fold_loaded_from_history(&history, str::to_string);
        assert_eq!(
            state
                .loaded()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    /// The records keep a large answer as a blob reference, which `expand`
    /// resolves before it is read.
    #[test]
    fn a_select_tools_answer_kept_as_a_blob_is_expanded_first() {
        let history = [
            Message::assistant_tool_calls(vec![lm_bridge::message::ToolCall::new(
                "call-1",
                SELECT_TOOLS_TOOL_NAME,
            )]),
            Message::tool_result("call-1", r#"<blob id="x"/>"#),
        ];
        let mut state = ToolDisclosureState::new();
        state.fold_loaded_from_history(&history, |text| {
            text.replace(r#"<blob id="x"/>"#, "Loaded: big")
        });
        assert!(state.loaded().contains("big"));
    }

    /// Build an availability map from `(name, description)` pairs.
    fn available<const N: usize>(entries: [(&str, &str); N]) -> BTreeMap<String, String> {
        entries
            .into_iter()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect()
    }

    #[test]
    fn test_announcement_on_first_call() {
        let mut state = ToolDisclosureState::new();
        let msgs =
            state.take_announcement_messages(&available([("read_db", ""), ("search_web", "")]));
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].text_content().contains("<tools_added>"));
        assert!(msgs[0].text_content().contains("read_db"));
    }

    #[test]
    fn test_no_announcement_when_unchanged() {
        let mut state = ToolDisclosureState::new();
        let tools = available([("a", "")]);
        state.take_announcement_messages(&tools);
        let msgs = state.take_announcement_messages(&tools);
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_announcement_reports_removals() {
        let mut state = ToolDisclosureState::new();
        state.take_announcement_messages(&available([("a", ""), ("b", "")]));
        let msgs = state.take_announcement_messages(&available([("a", "")]));
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].text_content().contains("<tools_removed>"));
        assert!(msgs[0].text_content().contains("b"));
    }

    #[test]
    fn test_announcement_includes_descriptions() {
        let mut state = ToolDisclosureState::new();
        let msgs = state.take_announcement_messages(&available([(
            "mcp__devscriptor__semantic_search",
            "Semantic (vector) search using embeddings",
        )]));
        let text = msgs[0].text_content();
        assert!(text.contains(
            "mcp__devscriptor__semantic_search — Semantic (vector) search using embeddings"
        ));
    }

    #[test]
    fn test_announcement_groups_by_mcp_server() {
        let mut state = ToolDisclosureState::new();
        let msgs = state.take_announcement_messages(&available([
            ("mcp__devscriptor__get_hover", "Hover info"),
            ("mcp__zai_web_search__web_search", "Search the web"),
            ("local_tool", "A local dynamic tool"),
        ]));
        let text = msgs[0].text_content();
        assert!(text.contains("## devscriptor"));
        assert!(text.contains("## zai_web_search"));
        // Non-MCP dynamic tools stay ungrouped and are listed before headers.
        let local = text.find("local_tool").unwrap();
        assert!(local < text.find("## devscriptor").unwrap());
    }

    #[test]
    fn test_description_is_truncated_and_single_line() {
        let long = "x".repeat(MAX_DESCRIPTION_LEN + 50);
        let line = render_tool_line("t", &format!("{long}\nsecond line"));
        assert!(line.ends_with('…'));
        assert!(!line.contains("second line"));
        assert!(line.chars().count() < MAX_DESCRIPTION_LEN + 20);
    }

    #[test]
    fn test_mcp_server_of() {
        assert_eq!(
            mcp_server_of("mcp__devscriptor__get_hover"),
            Some("devscriptor")
        );
        assert_eq!(mcp_server_of("plain_tool"), None);
        assert_eq!(mcp_server_of("mcp__nosuffix"), None);
    }

    #[test]
    fn test_fold_announcements_ignores_descriptions_and_headers() {
        let mut state = ToolDisclosureState::new();
        let history = vec![Message::system(
            "<tools_added>\n## devscriptor\nmcp__devscriptor__a — does a thing\nmcp__devscriptor__b — does b\n</tools_added>",
        )];
        state.fold_announcements_from_history(&history);
        assert!(state.announced.contains("mcp__devscriptor__a"));
        assert!(state.announced.contains("mcp__devscriptor__b"));
        assert_eq!(state.announced.len(), 2);
    }

    #[test]
    fn test_round_trip_announcement_then_fold_is_stable() {
        // Rendering an announcement and folding it back must yield no re-announcement.
        let tools = available([
            (
                "mcp__devscriptor__scan_codebase",
                "Scan a codebase and build the entity database",
            ),
            ("local_tool", ""),
        ]);
        let mut state = ToolDisclosureState::new();
        let msgs = state.take_announcement_messages(&tools);

        let mut resumed = ToolDisclosureState::new();
        resumed.fold_announcements_from_history(&msgs);
        assert!(resumed.take_announcement_messages(&tools).is_empty());
    }

    #[test]
    fn test_load_and_unload() {
        let mut state = ToolDisclosureState::new();
        state.load(["a", "b"]);
        assert!(state.loaded().contains("a"));
        state.unload("a");
        assert!(!state.loaded().contains("a"));
    }

    #[test]
    fn test_fold_announcements_from_history() {
        let mut state = ToolDisclosureState::new();
        let history = vec![
            Message::system("<tools_added>\na\nb\n</tools_added>"),
            Message::system("<tools_removed>\na\n</tools_removed>"),
        ];
        state.fold_announcements_from_history(&history);
        assert!(state.announced.contains("b"));
        assert!(!state.announced.contains("a"));
    }

    /// With nothing loadable at all, the empty state is announced exactly
    /// once.
    #[test]
    fn empty_loadable_set_is_announced_once() {
        let mut state = ToolDisclosureState::new();
        let empty = std::collections::BTreeMap::new();
        let msgs = state.take_announcement_messages(&empty);
        assert_eq!(msgs.len(), 1);
        let text = msgs[0].text_content();
        assert!(text.contains(EMPTY_ANNOUNCEMENT_MARKER), "{text}");
        assert!(text.contains("Do not call select_tools"), "{text}");
        assert!(!text.contains("Use the select_tools tool"), "{text}");
        assert!(state.take_announcement_messages(&empty).is_empty());
    }

    /// The empty announcement is folded back from a resumed history, so a
    /// resumed session does not hear it again.
    #[test]
    fn empty_announcement_survives_resume() {
        let mut state = ToolDisclosureState::new();
        let empty = std::collections::BTreeMap::new();
        let msgs = state.take_announcement_messages(&empty);

        let mut resumed = ToolDisclosureState::new();
        resumed.fold_announcements_from_history(&msgs);
        assert!(
            resumed.take_announcement_messages(&empty).is_empty(),
            "the folded history already announced the empty state"
        );
    }

    /// A session whose history has announcements but no empty-state marker
    /// hears the empty state once, then stays silent.
    #[test]
    fn legacy_history_without_the_marker_announces_empty_once() {
        let mut state = ToolDisclosureState::new();
        let history = vec![
            Message::system("<tools_added>\nmcp__srv__a\n</tools_added>"),
            Message::system("<tools_removed>\nmcp__srv__a\n</tools_removed>"),
        ];
        state.fold_announcements_from_history(&history);
        let empty = std::collections::BTreeMap::new();
        let msgs = state.take_announcement_messages(&empty);
        assert_eq!(msgs.len(), 1, "the legacy announcements lack the marker");
        assert!(msgs[0].text_content().contains(EMPTY_ANNOUNCEMENT_MARKER));
        assert!(state.take_announcement_messages(&empty).is_empty());
    }

    /// Removing the last loadable tool announces the removal together with
    /// the empty state, and only once.
    #[test]
    fn removing_the_last_tool_announces_the_empty_state() {
        let mut state = ToolDisclosureState::new();
        state.take_announcement_messages(&available([("a", "")]));
        let empty = std::collections::BTreeMap::new();
        let msgs = state.take_announcement_messages(&empty);
        assert_eq!(msgs.len(), 1);
        let text = msgs[0].text_content();
        assert!(
            text.contains("<tools_removed>\na\n</tools_removed>"),
            "{text}"
        );
        assert!(text.contains(EMPTY_ANNOUNCEMENT_MARKER), "{text}");
        assert!(state.take_announcement_messages(&empty).is_empty());
    }

    /// The empty announcement is not repeated when the history loses it
    /// (compaction): it stays silent until a `<tools_added>` announcement
    /// names new tools, and a session resumed from a history without the
    /// marker announces it once via the fold path.
    #[test]
    fn forgetting_announcements_keeps_the_empty_state_silent() {
        let mut state = ToolDisclosureState::new();
        let empty = std::collections::BTreeMap::new();
        state.take_announcement_messages(&empty);
        state.forget_announcements();
        assert!(
            state.take_announcement_messages(&empty).is_empty(),
            "no trailing announcement after a compaction"
        );
    }

    /// A per-name failure record counts up to and past the escalation
    /// threshold; a successful name clears only its own record; a success in
    /// the batch resets the consecutive streak but leaves the failed
    /// siblings' counts alone.
    #[test]
    fn record_selection_outcome_tracks_names_and_the_streak() {
        let mut state = ToolDisclosureState::new();
        assert!(
            state
                .record_selection_outcome(&[], &["ghost".to_string()])
                .is_empty()
        );
        let escalated = state.record_selection_outcome(&[], &["ghost".to_string()]);
        assert_eq!(escalated, [("ghost".to_string(), 2)]);
        assert_eq!(state.consecutive_failures(), 2);

        // A batch with a success: the streak resets, the failed sibling
        // keeps counting, the successful name's record clears.
        let escalated =
            state.record_selection_outcome(&["dyn_a".to_string()], &["ghost".to_string()]);
        assert_eq!(escalated, [("ghost".to_string(), 3)]);
        assert_eq!(state.consecutive_failures(), 0);
        let escalated = state.record_selection_outcome(&[], &["dyn_a".to_string()]);
        assert!(escalated.is_empty(), "dyn_a succeeded last time");
    }

    /// Case variants of a name share one failure record and count once per
    /// call, but every variant is escalated.
    #[test]
    fn failure_records_are_case_insensitive() {
        let mut state = ToolDisclosureState::new();
        let failed: Vec<String> = ["Ghost".to_string(), "ghost".to_string()].to_vec();
        assert!(state.record_selection_outcome(&[], &failed).is_empty());
        let escalated = state.record_selection_outcome(&[], &failed);
        assert_eq!(
            escalated,
            [("Ghost".to_string(), 2), ("ghost".to_string(), 2)]
        );
    }

    /// The fingerprint is registration-order independent but reacts to a
    /// name's dynamic flag changing; a change resets the counters.
    #[test]
    fn failure_counters_reset_on_a_fingerprint_change() {
        struct Dummy {
            def: lm_bridge::tool::Tool,
        }
        #[async_trait::async_trait]
        impl crate::agent_loop::types::ExecutableTool for Dummy {
            fn tool_definition(&self) -> &lm_bridge::tool::Tool {
                &self.def
            }
            fn accesses(&self, _: &serde_json::Value) -> crate::agent_loop::types::ToolAccesses {
                crate::agent_loop::types::ToolAccesses::none()
            }
            fn approval_rule(&self, _: &serde_json::Value) -> &str {
                "silent"
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _: &crate::agent_loop::types::ExecutableToolContext,
                _: &str,
            ) -> crate::agent_loop::types::ExecutableToolResult {
                crate::agent_loop::types::ExecutableToolResult::text("")
            }
        }
        fn dummy(name: &str) -> Arc<Dummy> {
            Arc::new(Dummy {
                def: lm_bridge::tool::Tool::new(name, "", serde_json::json!({"type": "object"})),
            })
        }

        let mut registry = ToolRegistry::new();
        registry.register(dummy("a"));
        let first = ToolDisclosureState::selection_fingerprint(&registry);
        let mut reordered = ToolRegistry::new();
        reordered.register(dummy("a"));
        assert_eq!(
            first,
            ToolDisclosureState::selection_fingerprint(&reordered)
        );

        let mut state = ToolDisclosureState::new();
        state.reset_failures_if_selection_changed(&first);
        state.record_selection_outcome(&[], &["ghost".to_string()]);
        state.record_selection_outcome(&[], &["ghost".to_string()]);
        assert_eq!(state.consecutive_failures(), 2);

        // Static → dynamic changes the selection outcome for the name.
        reordered.register_dynamic(dummy("a"));
        let changed = ToolDisclosureState::selection_fingerprint(&reordered);
        assert_ne!(first, changed);
        state.reset_failures_if_selection_changed(&changed);
        assert_eq!(state.consecutive_failures(), 0);
        assert!(
            state
                .record_selection_outcome(&[], &["ghost".to_string()])
                .is_empty(),
            "the counter reset starts ghost from one attempt"
        );
    }
}
