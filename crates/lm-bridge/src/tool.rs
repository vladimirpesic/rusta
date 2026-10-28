use serde::{Deserialize, Serialize};

/// A tool that can be exposed to an LLM.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema object describing the parameters.
    pub parameters: serde_json::Value,
}

impl Tool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// Add a property to the parameter schema, or nothing when `schema` is `None`.
    ///
    /// A no-op `None` is the point: it lets a tool advertise a parameter only
    /// when there is actually a choice to make. Advertising one whose sole legal
    /// value is the status quo invites the model to reason about a decision it
    /// does not have.
    #[must_use]
    pub fn with_optional_property(mut self, name: &str, schema: Option<serde_json::Value>) -> Self {
        if let Some(schema) = schema
            && let Some(properties) = self
                .parameters
                .get_mut("properties")
                .and_then(serde_json::Value::as_object_mut)
        {
            properties.insert(name.to_string(), schema);
        }
        self
    }

    /// Returns true if this is a Moonshot builtin function (e.g. `$web_search`).
    pub fn is_builtin_function(&self) -> bool {
        self.name.starts_with('$')
    }
}
