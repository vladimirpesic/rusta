pub mod builtin;

use std::{collections::HashMap, collections::HashSet, sync::Arc};

use lm_bridge::tool::Tool;

use crate::agent_loop::types::ExecutableTool;

pub struct ToolRegistry {
    tools: Vec<Arc<dyn ExecutableTool>>,
    by_name: HashMap<String, usize>,
    dynamic_names: HashSet<String>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            by_name: HashMap::new(),
            dynamic_names: HashSet::new(),
        }
    }

    /// Register a static tool that is always advertised in the top-level
    /// tool table. A tool registered dynamic before becomes static.
    pub fn register(&mut self, tool: Arc<dyn ExecutableTool>) {
        let name = tool.tool_definition().name.clone();
        self.dynamic_names.remove(&name);
        self.insert(name, tool);
    }

    /// Register a dynamic tool. Its definition is withheld from the initial
    /// tool table; the model must call `select_tools` to load it. A tool
    /// registered static before becomes dynamic.
    pub fn register_dynamic(&mut self, tool: Arc<dyn ExecutableTool>) {
        let name = tool.tool_definition().name.clone();
        self.dynamic_names.insert(name.clone());
        self.insert(name, tool);
    }

    fn insert(&mut self, name: String, tool: Arc<dyn ExecutableTool>) {
        if let Some(&idx) = self.by_name.get(&name) {
            // Replace the existing definition in-place so callers never see
            // duplicate tool entries.
            self.tools[idx] = tool;
        } else {
            let idx = self.tools.len();
            self.tools.push(tool);
            self.by_name.insert(name, idx);
        }
    }

    /// Drop every tool. A fork rebuilds its table as a copy of its caller's,
    /// where unregistering one by one would scramble the order.
    pub fn clear(&mut self) {
        self.tools.clear();
        self.by_name.clear();
        self.dynamic_names.clear();
    }

    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
        let idx = self.by_name.remove(name)?;
        self.dynamic_names.remove(name);
        // Swap the last tool into the removed slot to keep the index map valid.
        if idx == self.tools.len() - 1 {
            self.tools.pop()
        } else {
            let last = self.tools.pop()?;
            let old = std::mem::replace(&mut self.tools[idx], last);
            let moved_name = self.tools[idx].tool_definition().name.clone();
            self.by_name.insert(moved_name, idx);
            Some(old)
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn ExecutableTool>> {
        self.by_name.get(name).map(|&idx| self.tools[idx].clone())
    }

    pub fn definitions(&self) -> Vec<&Tool> {
        self.tools.iter().map(|t| t.tool_definition()).collect()
    }

    /// Definitions of static tools only.
    pub fn static_definitions(&self) -> Vec<&Tool> {
        self.tools
            .iter()
            .filter(|t| !self.dynamic_names.contains(&t.tool_definition().name))
            .map(|t| t.tool_definition())
            .collect()
    }

    /// Definitions of dynamic tools.
    pub fn dynamic_definitions(&self) -> Vec<&Tool> {
        self.tools
            .iter()
            .filter(|t| self.dynamic_names.contains(&t.tool_definition().name))
            .map(|t| t.tool_definition())
            .collect()
    }

    pub fn dynamic_definition_names(&self) -> Vec<&str> {
        self.dynamic_names.iter().map(String::as_str).collect()
    }

    pub fn is_dynamic(&self, name: &str) -> bool {
        self.dynamic_names.contains(name)
    }

    pub fn all(&self) -> &[Arc<dyn ExecutableTool>] {
        &self.tools
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::Value;

    use crate::agent_loop::types::{ExecutableToolContext, ExecutableToolResult, ToolAccesses};

    struct DummyTool {
        definition: Tool,
    }

    impl DummyTool {
        fn named(name: &str) -> Self {
            Self {
                definition: Tool::new(name.to_string(), "", Value::Object(Default::default())),
            }
        }
    }

    #[async_trait]
    impl ExecutableTool for DummyTool {
        fn tool_definition(&self) -> &Tool {
            &self.definition
        }

        fn accesses(&self, _input: &Value) -> ToolAccesses {
            ToolAccesses::none()
        }

        fn approval_rule(&self, _input: &Value) -> &str {
            "silent"
        }

        async fn execute(
            &self,
            _input: Value,
            _ctx: &ExecutableToolContext,
            _tool_call_id: &str,
        ) -> ExecutableToolResult {
            ExecutableToolResult::text("")
        }
    }

    #[test]
    fn test_clear_empties_the_registry() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool::named("a")));
        registry.register_dynamic(Arc::new(DummyTool::named("b")));

        registry.clear();

        assert!(registry.all().is_empty());
        assert!(registry.definitions().is_empty());
        assert!(registry.dynamic_definition_names().is_empty());
        assert!(registry.get("a").is_none());
    }

    #[test]
    fn test_unregister_removes_tool_and_keeps_index_valid() {
        let mut registry = ToolRegistry::new();
        for name in ["a", "b", "c"] {
            registry.register(Arc::new(DummyTool::named(name)));
        }

        let removed = registry.unregister("b").unwrap();
        assert_eq!(removed.tool_definition().name, "b");

        assert!(registry.get("b").is_none());
        assert!(registry.get("a").is_some());
        assert!(registry.get("c").is_some());
        assert_eq!(registry.definitions().len(), 2);
    }

    #[test]
    fn test_register_replaces_duplicate_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool::named("a")));
        registry.register(Arc::new(DummyTool::named("a")));

        assert_eq!(registry.definitions().len(), 1);
        assert_eq!(registry.all().len(), 1);
        assert!(registry.get("a").is_some());
    }

    #[test]
    fn test_dynamic_tools_are_separate_from_static() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(DummyTool::named("static_a")));
        registry.register_dynamic(Arc::new(DummyTool::named("dynamic_b")));

        assert!(registry.is_dynamic("dynamic_b"));
        assert!(!registry.is_dynamic("static_a"));
        assert_eq!(registry.static_definitions().len(), 1);
        assert_eq!(registry.dynamic_definitions().len(), 1);
        assert!(
            registry
                .static_definitions()
                .iter()
                .all(|t| t.name == "static_a")
        );
    }

    #[test]
    fn test_unregister_dynamic_removes_dynamic_flag() {
        let mut registry = ToolRegistry::new();
        registry.register_dynamic(Arc::new(DummyTool::named("dynamic_b")));
        registry.unregister("dynamic_b");

        assert!(registry.get("dynamic_b").is_none());
        assert!(registry.dynamic_definitions().is_empty());
    }
}
