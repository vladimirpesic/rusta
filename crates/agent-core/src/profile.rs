use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::errors::AgentError;

/// A reusable subagent personality.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    /// Name of the profile this one inherits from.
    pub extends: Option<String>,
    /// Human-readable description.
    pub description: Option<String>,
    /// When to delegate to it, for the agent choosing a type: shown with the
    /// description and its tools in the `agent` tool's description
    /// ([`ProfileRegistry::describe_types`]). Unset inherits from
    /// `extends`.
    pub when_to_use: Option<String>,
    /// System prompt used for the subagent.
    pub system_prompt: Option<String>,
    /// Variables substituted into the system prompt or task template.
    #[serde(default)]
    pub prompt_variables: HashMap<String, String>,
    /// Tool names the subagent is allowed to use. Empty means every tool that
    /// `denied_tools` does not remove. Provider built-ins (`$web_search` on
    /// Kimi) are never cut by either list — the provider executes them
    /// server-side (C4).
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Tool names removed from the subagent's table however long the table
    /// is: the "everything except …" half of a profile. Combined with
    /// `allowed_tools` (the intersection survives).
    #[serde(default)]
    pub denied_tools: Vec<String>,
    /// Prepend the repository's git context (branch, dirty files, recent
    /// commits) to the subagent's first message. Nothing is prepended outside
    /// a work tree.
    #[serde(default)]
    pub git_context: bool,
    /// Which of the session's MCP tools (`mcp__*`) the subagent gets, deferred
    /// as they are for the session's agent. Unset inherits from `extends`, and
    /// is none at the root. `coder` and `default` take them all, and the
    /// read-only `explore` and `plan` the read-only ones, so the agents that
    /// research code can search it with Devscriptor.
    pub mcp_tools: Option<McpTools>,
}

impl Profile {
    /// Which of the session's MCP tools this profile's subagents get.
    #[must_use]
    pub fn mcp_access(&self) -> McpTools {
        self.mcp_tools.unwrap_or(McpTools::None)
    }

    /// Its tools in words: those it lists, less those it denies, or every
    /// tool but those it denies; then which of the session's MCP tools it
    /// gets. Said by rule, not by name: tools are registered after the
    /// `agent` tool is, and the session's MCP tools when their servers
    /// connect.
    fn tools_summary(&self) -> String {
        let listed: Vec<&str> = self
            .allowed_tools
            .iter()
            .filter(|tool| !self.denied_tools.contains(tool))
            .map(String::as_str)
            .collect();
        let builtins = if !self.allowed_tools.is_empty() {
            if listed.is_empty() {
                "none".to_string()
            } else {
                listed.join(", ")
            }
        } else if self.denied_tools.is_empty() {
            "every tool".to_string()
        } else {
            format!("every tool but {}", self.denied_tools.join(", "))
        };
        match self.mcp_access() {
            McpTools::None => builtins,
            McpTools::ReadOnly => format!("{builtins}; the session's read-only MCP tools"),
            McpTools::All => format!("{builtins}; the session's MCP tools"),
        }
    }
}

/// Which of the session's MCP tools a profile's subagents get. A profile file
/// writes it as `mcp_tools: true` (all), `false` (none) or `read_only`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTools {
    /// None of them.
    None,
    /// Those whose server declares them read-only (`readOnlyHint`):
    /// [`crate::agent_loop::types::ExecutableTool::is_read_only`].
    ReadOnly,
    /// Every one.
    All,
}

impl Serialize for McpTools {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::None => serializer.serialize_bool(false),
            Self::ReadOnly => serializer.serialize_str("read_only"),
            Self::All => serializer.serialize_bool(true),
        }
    }
}

impl<'de> Deserialize<'de> for McpTools {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Setting;
        impl serde::de::Visitor<'_> for Setting {
            type Value = McpTools;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("true, false or read_only")
            }

            fn visit_bool<E: serde::de::Error>(self, all: bool) -> Result<McpTools, E> {
                Ok(if all { McpTools::All } else { McpTools::None })
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<McpTools, E> {
                match value {
                    "read_only" => Ok(McpTools::ReadOnly),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(value), &self)),
                }
            }
        }
        deserializer.deserialize_any(Setting)
    }
}

/// The prompt guidance for the session's MCP tools — Devscriptor's workflow —
/// in the form each [`McpTools`] needs: a sub-agent with only the read-only
/// tools is not told to scan, edit or record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpGuidance {
    /// For a sub-agent with every MCP tool; the session's own prompt carries
    /// the same.
    pub all: String,
    /// For a sub-agent with the read-only ones.
    pub read_only: String,
}

impl McpGuidance {
    /// The guidance for a sub-agent that gets `tools`.
    #[must_use]
    pub fn for_tools(&self, tools: McpTools) -> Option<&str> {
        match tools {
            McpTools::None => None,
            McpTools::ReadOnly => Some(&self.read_only),
            McpTools::All => Some(&self.all),
        }
    }
}

/// The `default` profile's role.
const DEFAULT_ROLE: &str = "You take on a delegated task and see it through with the tools you \
     have. Stay within what the task asks for.";

/// The `explore` profile's role.
const EXPLORE_ROLE: &str = "You are a codebase exploration specialist: you search, read and \
     analyse, and you change nothing.

- When code-intelligence tools are announced (`<tools_added>`), search and navigate with them \
  first: they answer from an index of the whole workspace. Read files for the detail they \
  point to.
- Use `glob` to find files by pattern, anchored with an extension or a directory; `grep` to \
  search contents with a regular expression; and `read_file` when you know the path.
- Use `bash` only for read-only commands such as `ls`, `find`, `git status`, `git log` and \
  `git diff`. Never create, modify or delete a file.
- When `web_search` or `fetch` is available, use it when the question needs outside context, \
  such as library documentation or an error message; the local code stays your primary source.
- Match the depth of your search to the thoroughness the caller asks for, and run \
  independent searches and reads in parallel.
- If your task opens with a <git-context> block, use it to orient yourself about the \
  repository's state before starting your investigation.

Be fast. Report what you found in a clear structure, with file paths and line numbers.";

/// The `plan` profile's role.
const PLAN_ROLE: &str = "Before designing your implementation plan, consider whether you fully \
     understand the codebase areas relevant to the task. If not, recommend the parent agent use \
     the explore agent (profile=\"explore\") to investigate key questions first. In your \
     response, clearly state:
1. What you already know from the information provided
2. What questions remain unanswered that would benefit from explore agent investigation
3. Your implementation plan (either preliminary if questions remain, or final if sufficient \
     context exists)

You are a read-only planning agent: you can read and search files — with the code-intelligence \
tools first, when they are announced — and, when web tools are available, consult the web, but \
you have no shell and no file-editing tools. Where the general instructions tell you to make \
changes with tools, that does not apply to you — do not attempt to run commands or modify \
files. Your deliverable is the plan itself, returned as your final message.";

/// The `coder` profile's role.
const CODER_ROLE: &str = "You are a software engineering sub-agent: you read code, edit it, \
     run commands and verify the result.

Your final message is the entire handoff. Make it technically complete: what you changed and \
why, the path of every file you touched, how you verified the change (the tests or commands \
you ran, with their results), and anything left undone or worth a follow-up. If you are \
stopped before you finish, your caller gets only what you have written so far, so keep the \
handoff current.";

/// Registry of profiles with inheritance resolution.
#[derive(Debug, Clone, Default)]
pub struct ProfileRegistry {
    profiles: HashMap<String, Profile>,
}

impl ProfileRegistry {
    pub fn new() -> Self {
        Self {
            profiles: HashMap::new(),
        }
    }

    /// Create a registry pre-populated with the built-in profiles.
    ///
    /// The tool lists (the provider's own web search is never listed — no
    /// profile cuts a provider built-in):
    ///
    /// | Profile | Tools |
    /// | --- | --- |
    /// | `explore` | `bash`, `read_file`, `read_media`, `glob`, `grep`, `web_search`, `fetch` and the read-only MCP tools — read-only by prompt, plus the git-context prefix |
    /// | `plan` | `read_file`, `read_media`, `glob`, `grep`, `web_search`, `fetch` and the read-only MCP tools — no shell |
    /// | `coder` | everything except `agent`, `agent_swarm`, `ask_user`, `ask_user_question`, `exit_plan_mode` and the goal tools |
    /// | `default` | everything except the main-agent-only tools (`agent_swarm`, `ask_user`, `ask_user_question`, `exit_plan_mode`, the goal tools); `agent` stays — the depth bound, not the profile, gates nesting |
    ///
    /// Plan mode is the session agent's alone: only the user turns it on, so no
    /// sub-agent is ever in it to leave.
    pub fn with_builtin_profiles() -> Self {
        let mut registry = Self::new();
        registry.register(Profile {
            name: "default".to_string(),
            description: Some("General-purpose subagent".to_string()),
            system_prompt: Some(DEFAULT_ROLE.to_string()),
            denied_tools: [
                "agent_swarm",
                "ask_user",
                "ask_user_question",
                "create_goal",
                "exit_plan_mode",
                "get_goal",
                "set_goal_budget",
                "update_goal",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            mcp_tools: Some(McpTools::All),
            ..Default::default()
        });
        registry.register(Profile {
            name: "explore".to_string(),
            description: Some(
                "Read-only codebase exploration: finds files, searches code and answers \
                 questions about it, and changes nothing"
                    .to_string(),
            ),
            when_to_use: Some(
                "Fast agent specialized for exploring codebases: it finds files by patterns \
                 (e.g. \"src/**/*.yaml\"), searches code for keywords (e.g. \"database \
                 connection\"), and answers questions about the codebase (e.g. \"how does \
                 the auth module work?\"). When calling this agent, specify the desired \
                 thoroughness level: \"quick\" for basic searches, \"medium\" for moderate \
                 exploration, or \"thorough\" for comprehensive analysis across multiple \
                 locations and naming conventions."
                    .to_string(),
            ),
            system_prompt: Some(EXPLORE_ROLE.to_string()),
            allowed_tools: [
                "bash",
                "read_file",
                "read_media",
                "glob",
                "grep",
                "web_search",
                "fetch",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            git_context: true,
            mcp_tools: Some(McpTools::ReadOnly),
            ..Default::default()
        });
        registry.register(Profile {
            name: "plan".to_string(),
            description: Some(
                "Read-only implementation planning and architecture design".to_string(),
            ),
            when_to_use: Some(
                "Use this agent when the parent agent needs a step-by-step implementation \
                 plan, key file identification, and architectural trade-off analysis before \
                 code changes are made."
                    .to_string(),
            ),
            system_prompt: Some(PLAN_ROLE.to_string()),
            allowed_tools: [
                "read_file",
                "read_media",
                "glob",
                "grep",
                "web_search",
                "fetch",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            mcp_tools: Some(McpTools::ReadOnly),
            ..Default::default()
        });
        registry.register(Profile {
            name: "coder".to_string(),
            description: Some(
                "Software engineering: reads, edits and verifies code, and hands back a \
                 complete account of the change"
                    .to_string(),
            ),
            when_to_use: Some(
                "Use this agent for non-trivial software engineering work that may require \
                 reading files, editing code, running commands, and returning a compact but \
                 technically complete summary to the parent agent."
                    .to_string(),
            ),
            system_prompt: Some(CODER_ROLE.to_string()),
            denied_tools: [
                "agent",
                "agent_swarm",
                "ask_user",
                "ask_user_question",
                "create_goal",
                "exit_plan_mode",
                "get_goal",
                "set_goal_budget",
                "update_goal",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            mcp_tools: Some(McpTools::All),
            ..Default::default()
        });
        registry
    }

    /// Register a profile, overwriting any existing profile with the same name.
    pub fn register(&mut self, profile: Profile) {
        self.profiles.insert(profile.name.clone(), profile);
    }

    /// Load all `.yaml` / `.yml` files from a directory into the registry.
    pub fn load_dir(&mut self, dir: impl AsRef<Path>) -> Result<(), AgentError> {
        let dir = dir.as_ref();
        if !dir.exists() {
            return Ok(());
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| AgentError::Config(format!("failed to read profile dir {dir:?}: {e}")))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext == "yaml" || ext == "yml")
                    .unwrap_or(false)
            })
            .collect();
        entries.sort();

        for path in entries {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| AgentError::Config(format!("failed to read profile {path:?}: {e}")))?;
            let mut profile: Profile = serde_yaml::from_str(&content).map_err(|e| {
                AgentError::Config(format!("failed to parse profile {path:?}: {e}"))
            })?;
            if profile.name.is_empty() {
                profile.name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unknown".to_string());
            }
            self.register(profile);
        }
        Ok(())
    }

    /// Load profiles from multiple directories, with later directories taking
    /// precedence over earlier ones.
    pub fn load_dirs(dirs: &[PathBuf]) -> Result<Self, AgentError> {
        let mut registry = Self::with_builtin_profiles();
        for dir in dirs {
            registry.load_dir(dir)?;
        }
        Ok(registry)
    }

    /// Return a shallow reference to a profile, if it exists.
    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    /// Resolve a profile, recursively merging inherited fields.
    ///
    /// Returns `None` if the profile is missing or part of an inheritance cycle.
    pub fn resolve(&self, name: &str) -> Option<Profile> {
        let mut visited = HashSet::new();
        self.resolve_with_visited(name, &mut visited)
    }

    fn resolve_with_visited(&self, name: &str, visited: &mut HashSet<String>) -> Option<Profile> {
        if !visited.insert(name.to_string()) {
            return None;
        }

        let child = self.profiles.get(name)?.clone();
        let Some(parent_name) = &child.extends else {
            return Some(child);
        };

        let parent = self.resolve_with_visited(parent_name, visited)?;
        Some(merge_profiles(&parent, &child))
    }

    /// Return the names of all registered profiles.
    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.profiles.keys().cloned().collect();
        names.sort();
        names
    }

    /// The agent types, as the `agent` tool describes them: each name with its
    /// description and when to use it, then its tools
    /// (`Profile::tools_summary`). An agent choosing a type by name alone
    /// would brief `explore` sub-agents to read every file with `read_file`
    /// and `grep`, unaware that they have the code-intelligence tools.
    #[must_use]
    pub fn describe_types(&self) -> String {
        self.names()
            .iter()
            .filter_map(|name| self.resolve(name))
            .map(|profile| {
                let mut details = String::new();
                for detail in [
                    profile.description.as_deref(),
                    profile.when_to_use.as_deref(),
                ]
                .into_iter()
                .flatten()
                .filter(|detail| !detail.is_empty())
                {
                    if !details.is_empty() {
                        if !details.ends_with('.') {
                            details.push('.');
                        }
                        details.push(' ');
                    }
                    details.push_str(detail);
                }
                let header = if details.is_empty() {
                    format!("- {}", profile.name)
                } else {
                    format!("- {}: {details}", profile.name)
                };
                format!("{header}\n  Tools: {}", profile.tools_summary())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn merge_profiles(parent: &Profile, child: &Profile) -> Profile {
    let mut prompt_variables = parent.prompt_variables.clone();
    prompt_variables.extend(child.prompt_variables.clone());

    let mut allowed_tools = parent.allowed_tools.clone();
    for tool in &child.allowed_tools {
        if !allowed_tools.contains(tool) {
            allowed_tools.push(tool.clone());
        }
    }
    // A denial inherited is a denial kept: the child's list is added to the
    // parent's, never subtracted from it.
    let mut denied_tools = parent.denied_tools.clone();
    for tool in &child.denied_tools {
        if !denied_tools.contains(tool) {
            denied_tools.push(tool.clone());
        }
    }

    Profile {
        name: child.name.clone(),
        extends: None,
        description: child
            .description
            .clone()
            .or_else(|| parent.description.clone()),
        when_to_use: child
            .when_to_use
            .clone()
            .or_else(|| parent.when_to_use.clone()),
        system_prompt: child
            .system_prompt
            .clone()
            .or_else(|| parent.system_prompt.clone()),
        prompt_variables,
        allowed_tools,
        denied_tools,
        git_context: child.git_context || parent.git_context,
        mcp_tools: child.mcp_tools.or(parent.mcp_tools),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The `agent` tool shows each agent type with what it is for and what it
    /// has: an agent choosing a type by name alone would brief `explore`
    /// sub-agents to read every file with `read_file` and `grep`, unaware that
    /// they have the code-intelligence tools.
    #[test]
    fn the_agent_types_say_what_each_is_for_and_has() {
        let described = ProfileRegistry::with_builtin_profiles().describe_types();
        let lines: Vec<&str> = described.lines().collect();
        assert_eq!(lines.len(), 8, "{described}");
        assert!(lines[0].starts_with("- coder: Software engineering: reads, edits and verifies code, and hands back a complete account of the change. Use this agent for non-trivial"), "{described}");
        assert_eq!(
            lines[1],
            "  Tools: every tool but agent, agent_swarm, ask_user, ask_user_question, \
             create_goal, exit_plan_mode, get_goal, set_goal_budget, update_goal; the session's \
             MCP tools"
        );
        assert_eq!(lines[2], "- default: General-purpose subagent");
        assert!(lines[4].starts_with("- explore: Read-only codebase exploration: finds files, searches code and answers questions about it, and changes nothing. Fast agent specialized for exploring codebases:"), "{described}");
        // No type sells delegation: a type that did would send the agent to
        // it for anything past a few searches, several at once.
        for nudge in ["more than 3 search", "Prefer launching multiple"] {
            assert!(!described.contains(nudge), "{described}");
        }
        assert_eq!(
            lines[5],
            "  Tools: bash, read_file, read_media, glob, grep, web_search, fetch; \
             the session's read-only MCP tools"
        );
        assert!(
            lines[6].starts_with("- plan: Read-only implementation planning"),
            "{described}"
        );

        // The read-only types search with the code-intelligence tools first,
        // as the guidance appended for them says, rather than against it.
        let registry = ProfileRegistry::with_builtin_profiles();
        for name in ["explore", "plan"] {
            let role = registry.resolve(name).unwrap().system_prompt.unwrap();
            assert!(role.contains("code-intelligence tools"), "{name}: {role}");
        }

        // A profile's own type is described as it inherits: a description that
        // ends a sentence is not given a second period.
        let mut registry = ProfileRegistry::with_builtin_profiles();
        registry.register(Profile {
            name: "scout".to_string(),
            extends: Some("explore".to_string()),
            description: Some("Scouts a module.".to_string()),
            ..Default::default()
        });
        let described = registry.describe_types();
        assert!(
            described.contains("- scout: Scouts a module. Fast agent specialized"),
            "{described}"
        );
    }

    /// The built-in roles are written with line continuations; none may leak
    /// indentation or doubled spaces into the prompt.
    #[test]
    fn builtin_roles_read_cleanly() {
        for role in [DEFAULT_ROLE, EXPLORE_ROLE, PLAN_ROLE, CODER_ROLE] {
            assert!(!role.contains("  "), "{role}");
            assert!(role.lines().all(|l| l == l.trim_start()), "{role}");
            assert!(!role.contains("\n\n\n"), "{role}");
        }
        assert!(EXPLORE_ROLE.contains("read and analyse, and you change nothing."));
        assert!(EXPLORE_ROLE.contains("\n- Use `bash` only for read-only commands"));
        assert!(EXPLORE_ROLE.contains("<git-context>"));
        assert!(PLAN_ROLE.contains("you have no shell and no file-editing tools"));
        assert!(PLAN_ROLE.contains("profile=\"explore\""));
    }

    #[test]
    fn test_builtin_profiles_resolve() {
        let registry = ProfileRegistry::with_builtin_profiles();
        for name in ["default", "explore", "plan", "coder"] {
            assert!(registry.resolve(name).is_some(), "{name}");
        }
        assert!(registry.resolve("missing").is_none());
    }

    /// The split: `explore` is read-only plus `bash` for read-only commands;
    /// `plan` drops the shell; `coder` and `default` keep the working tools
    /// minus the delegation, goal and plan-review tools.
    #[test]
    fn builtin_profiles_carry_their_tool_lists() {
        let registry = ProfileRegistry::with_builtin_profiles();

        let explore = registry.resolve("explore").unwrap();
        for kept in [
            "bash",
            "read_file",
            "read_media",
            "glob",
            "grep",
            "web_search",
            "fetch",
        ] {
            assert!(
                explore.allowed_tools.contains(&kept.to_string()),
                "explore keeps {kept}"
            );
        }
        for cut in [
            "write_file",
            "edit_file",
            "agent",
            "agent_swarm",
            "cron_create",
        ] {
            assert!(
                !explore.allowed_tools.contains(&cut.to_string()),
                "explore cuts {cut}"
            );
        }
        assert!(
            explore.git_context,
            "explore orients itself with the repository's state"
        );
        assert!(explore.denied_tools.is_empty());

        let plan = registry.resolve("plan").unwrap();
        assert!(
            !plan.allowed_tools.contains(&"bash".to_string()),
            "plan has no shell"
        );
        assert!(plan.allowed_tools.contains(&"read_file".to_string()));
        assert!(plan.allowed_tools.contains(&"fetch".to_string()));

        let coder = registry.resolve("coder").unwrap();
        assert!(
            coder.allowed_tools.is_empty(),
            "coder is an everything-except list"
        );
        for cut in [
            "agent",
            "agent_swarm",
            "ask_user",
            "ask_user_question",
            "create_goal",
            "exit_plan_mode",
            "get_goal",
            "set_goal_budget",
            "update_goal",
        ] {
            assert!(
                coder.denied_tools.contains(&cut.to_string()),
                "coder cuts {cut}"
            );
        }
        assert!(!coder.denied_tools.contains(&"bash".to_string()));
        assert!(!coder.denied_tools.contains(&"write_file".to_string()));

        let default = registry.resolve("default").unwrap();
        for cut in [
            "agent_swarm",
            "ask_user",
            "ask_user_question",
            "create_goal",
            "exit_plan_mode",
            "get_goal",
            "set_goal_budget",
            "update_goal",
        ] {
            assert!(
                default.denied_tools.contains(&cut.to_string()),
                "default cuts {cut}"
            );
        }
        assert!(
            !default.denied_tools.contains(&"agent".to_string()),
            "nesting is the depth bound's job, not the default profile's"
        );
    }

    #[test]
    fn test_load_dir_uses_filename_when_name_missing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("custom.yaml");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"system_prompt: hello").unwrap();
        drop(file);

        let mut registry = ProfileRegistry::new();
        registry.load_dir(temp.path()).unwrap();
        assert!(registry.get("custom").is_some());
    }

    #[test]
    fn test_inheritance_merges_fields() {
        let mut registry = ProfileRegistry::new();
        registry.register(Profile {
            name: "base".to_string(),
            system_prompt: Some("base prompt".to_string()),
            prompt_variables: [("a".to_string(), "1".to_string())].into(),
            allowed_tools: vec!["read_file".to_string()],
            denied_tools: vec!["agent_swarm".to_string()],
            git_context: true,
            ..Default::default()
        });
        registry.register(Profile {
            name: "child".to_string(),
            extends: Some("base".to_string()),
            description: Some("child desc".to_string()),
            prompt_variables: [("b".to_string(), "2".to_string())].into(),
            allowed_tools: vec!["write_file".to_string()],
            denied_tools: vec!["create_goal".to_string()],
            ..Default::default()
        });

        let resolved = registry.resolve("child").unwrap();
        assert_eq!(resolved.system_prompt.as_deref(), Some("base prompt"));
        assert_eq!(resolved.description.as_deref(), Some("child desc"));
        assert_eq!(
            resolved.prompt_variables.get("a").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            resolved.prompt_variables.get("b").map(String::as_str),
            Some("2")
        );
        assert!(resolved.allowed_tools.contains(&"read_file".to_string()));
        assert!(resolved.allowed_tools.contains(&"write_file".to_string()));
        // Denials and the git-context flag inherit additively.
        assert!(resolved.denied_tools.contains(&"agent_swarm".to_string()));
        assert!(resolved.denied_tools.contains(&"create_goal".to_string()));
        assert!(resolved.git_context);
    }

    /// The types that change code get all of the session's MCP tools, the
    /// read-only ones only those that read; a profile inherits the choice
    /// unless it makes its own.
    #[test]
    fn mcp_tools_go_to_the_working_profiles_and_are_inherited() {
        let mut registry = ProfileRegistry::with_builtin_profiles();
        let access =
            |registry: &ProfileRegistry, name: &str| registry.resolve(name).unwrap().mcp_access();
        assert_eq!(access(&registry, "coder"), McpTools::All);
        assert_eq!(access(&registry, "default"), McpTools::All);
        assert_eq!(access(&registry, "explore"), McpTools::ReadOnly);
        assert_eq!(access(&registry, "plan"), McpTools::ReadOnly);

        registry.register(Profile {
            name: "reviewer".to_string(),
            extends: Some("coder".to_string()),
            ..Default::default()
        });
        registry.register(Profile {
            name: "sealed".to_string(),
            extends: Some("coder".to_string()),
            mcp_tools: Some(McpTools::None),
            ..Default::default()
        });
        registry.register(Profile {
            name: "plain".to_string(),
            ..Default::default()
        });
        assert_eq!(access(&registry, "reviewer"), McpTools::All);
        assert_eq!(access(&registry, "sealed"), McpTools::None);
        assert_eq!(access(&registry, "plain"), McpTools::None);
    }

    /// A profile file names the setting `true`, `false` or `read_only`, and
    /// is told what else it could say.
    #[test]
    fn a_profile_file_names_its_mcp_tools() {
        for (yaml, tools) in [
            ("mcp_tools: true", McpTools::All),
            ("mcp_tools: false", McpTools::None),
            ("mcp_tools: read_only", McpTools::ReadOnly),
        ] {
            let profile: Profile = serde_yaml::from_str(&format!("name: p\n{yaml}")).unwrap();
            assert_eq!(profile.mcp_tools, Some(tools), "{yaml}");
            let written = serde_yaml::to_string(&profile).unwrap();
            let read: Profile = serde_yaml::from_str(&written).unwrap();
            assert_eq!(read.mcp_tools, Some(tools), "{written}");
        }
        let err = serde_yaml::from_str::<Profile>("name: p\nmcp_tools: some").unwrap_err();
        assert!(
            err.to_string().contains("true, false or read_only"),
            "{err}"
        );
    }

    #[test]
    fn test_child_overrides_parent_prompt() {
        let mut registry = ProfileRegistry::new();
        registry.register(Profile {
            name: "base".to_string(),
            system_prompt: Some("base".to_string()),
            ..Default::default()
        });
        registry.register(Profile {
            name: "child".to_string(),
            extends: Some("base".to_string()),
            system_prompt: Some("child".to_string()),
            ..Default::default()
        });

        let resolved = registry.resolve("child").unwrap();
        assert_eq!(resolved.system_prompt.as_deref(), Some("child"));
    }

    #[test]
    fn test_cyclic_inheritance_returns_none() {
        let mut registry = ProfileRegistry::new();
        registry.register(Profile {
            name: "a".to_string(),
            extends: Some("b".to_string()),
            ..Default::default()
        });
        registry.register(Profile {
            name: "b".to_string(),
            extends: Some("a".to_string()),
            ..Default::default()
        });

        assert!(registry.resolve("a").is_none());
    }
}
