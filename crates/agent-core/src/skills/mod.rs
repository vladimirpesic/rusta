//! Skill framework: reusable Markdown prompts with YAML frontmatter.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub mod parser;
pub mod registry;

pub use registry::SkillRegistry;

/// The longest description the `skill` tool lists for a skill, in
/// characters.
const LISTED_DESCRIPTION_CHARS: usize = 250;

/// Classification of a skill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillType {
    /// Injected into the agent's system/context prompt.
    #[default]
    System,
    /// Triggered by a specific user request or tool call.
    User,
}

/// A reusable prompt authored as Markdown with YAML frontmatter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: Option<String>,
    pub skill_type: SkillType,
    pub when_to_use: Option<String>,
    pub disable_model_invocation: bool,
    pub content: String,
}

impl Skill {
    /// Render the skill content with the supplied argument substitutions.
    ///
    /// Substitutes `{{key}}` placeholders with the corresponding value from
    /// `args`. Placeholders without a matching argument are left unchanged.
    pub fn render(&self, args: &HashMap<String, String>) -> String {
        let mut output = self.content.clone();
        for (key, value) in args {
            let placeholder = format!("{{{{{}}}}}", key);
            output = output.replace(&placeholder, value);
        }
        output
    }

    /// The skill as the `skill` tool lists it:
    /// `- name: description`, then `  When to use: …` when the skill says.
    /// A skill without a description is described by its first line.
    #[must_use]
    pub fn listing(&self) -> String {
        let description = self
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .or_else(|| {
                self.content
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
            })
            .unwrap_or("No description provided.");
        let mut listing = format!("- {}: {}", self.name, shorten(description));
        if let Some(when) = self
            .when_to_use
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            listing.push_str("\n  When to use: ");
            listing.push_str(when);
        }
        listing
    }
}

/// `text` within [`LISTED_DESCRIPTION_CHARS`], ending in `…` where cut.
fn shorten(text: &str) -> String {
    if text.chars().count() <= LISTED_DESCRIPTION_CHARS {
        return text.to_string();
    }
    let mut short: String = text.chars().take(LISTED_DESCRIPTION_CHARS - 1).collect();
    short.push('…');
    short
}

/// Record of a single skill invocation/activation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillActivation {
    pub name: String,
    #[serde(default)]
    pub args: HashMap<String, String>,
    #[serde(with = "chrono::serde::ts_seconds")]
    pub activated_at: chrono::DateTime<chrono::Utc>,
}

/// Persisted list of skill activations for a session.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ActiveSkills {
    pub activations: Vec<SkillActivation>,
    #[serde(skip)]
    persist_path: Option<std::path::PathBuf>,
}

impl ActiveSkills {
    /// Create an empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the path used for persistence.
    pub fn set_persist_path(&mut self, path: impl Into<std::path::PathBuf>) {
        self.persist_path = Some(path.into());
    }

    /// Record a skill activation and persist if a path is configured.
    pub fn record(
        &mut self,
        name: impl Into<String>,
        args: HashMap<String, String>,
    ) -> Result<(), crate::errors::AgentError> {
        self.activations.push(SkillActivation {
            name: name.into(),
            args,
            activated_at: chrono::Utc::now(),
        });
        self.persist()
    }

    /// Persist activations to the configured sidecar path, if any.
    pub fn persist(&self) -> Result<(), crate::errors::AgentError> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        std::fs::write(path, json).map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
        Ok(())
    }

    /// Load activations from a sidecar file, and persist later ones to it.
    ///
    /// A missing file is a session that has used no skill yet: the tracker
    /// starts empty and creates the file at its first activation.
    pub fn load(path: &std::path::Path) -> Result<Self, crate::errors::AgentError> {
        let mut skills = if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?;
            serde_json::from_str(&content)
                .map_err(|e| crate::errors::AgentError::Config(e.to_string()))?
        } else {
            Self::new()
        };
        skills.persist_path = Some(path.to_path_buf());
        Ok(skills)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_render_substitutes_args() {
        let skill = Skill {
            name: "greet".to_string(),
            description: None,
            skill_type: SkillType::User,
            when_to_use: None,
            disable_model_invocation: false,
            content: "Hello {{name}}, welcome to {{place}}.".to_string(),
        };
        let mut args = HashMap::new();
        args.insert("name".to_string(), "Ada".to_string());
        args.insert("place".to_string(), "Rust".to_string());
        assert_eq!(skill.render(&args), "Hello Ada, welcome to Rust.");
    }

    #[test]
    fn test_skill_render_leaves_unknown_placeholders() {
        let skill = Skill {
            name: "partial".to_string(),
            description: None,
            skill_type: SkillType::System,
            when_to_use: None,
            disable_model_invocation: false,
            content: "Use {{known}} and {{unknown}}.".to_string(),
        };
        let mut args = HashMap::new();
        args.insert("known".to_string(), "X".to_string());
        assert_eq!(skill.render(&args), "Use X and {{unknown}}.");
    }

    fn skill(description: Option<&str>, when_to_use: Option<&str>, content: &str) -> Skill {
        Skill {
            name: "review".to_string(),
            description: description.map(str::to_string),
            skill_type: SkillType::User,
            when_to_use: when_to_use.map(str::to_string),
            disable_model_invocation: false,
            content: content.to_string(),
        }
    }

    /// A skill is listed with its description and when to use it; without
    /// a description, by its first line of text.
    #[test]
    fn a_skill_is_listed_by_what_it_says_of_itself() {
        assert_eq!(
            skill(Some("Review a diff."), Some("Before a commit"), "Body").listing(),
            "- review: Review a diff.\n  When to use: Before a commit"
        );
        assert_eq!(
            skill(None, None, "\n  Look for bugs.  \nThen style.").listing(),
            "- review: Look for bugs."
        );
        assert_eq!(
            skill(Some("  "), Some(""), "").listing(),
            "- review: No description provided."
        );
    }

    /// A long description is cut to the listing's limit, and says so.
    #[test]
    fn a_long_description_is_cut() {
        let listing = skill(Some(&"é".repeat(300)), None, "").listing();
        let description = listing.strip_prefix("- review: ").unwrap();
        assert_eq!(description.chars().count(), LISTED_DESCRIPTION_CHARS);
        assert!(description.ends_with("é…"), "{description}");
        let exact = "x".repeat(LISTED_DESCRIPTION_CHARS);
        assert_eq!(
            skill(Some(&exact), None, "").listing(),
            format!("- review: {exact}")
        );
    }

    #[test]
    fn test_active_skills_persist_and_load() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("skills.json");

        {
            let mut active = ActiveSkills::new();
            active.set_persist_path(&path);
            let mut args = HashMap::new();
            args.insert("topic".to_string(), "refactoring".to_string());
            active.record("code-review", args).unwrap();
        }

        let loaded = ActiveSkills::load(&path).unwrap();
        assert_eq!(loaded.activations.len(), 1);
        assert_eq!(loaded.activations[0].name, "code-review");
        assert_eq!(
            loaded.activations[0].args.get("topic").map(String::as_str),
            Some("refactoring")
        );
    }

    /// A sidecar that does not exist yet loads empty, and the first
    /// activation creates it.
    #[test]
    fn a_missing_sidecar_is_created_at_the_first_activation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("skills.json");

        let mut active = ActiveSkills::load(&path).unwrap();
        assert!(active.activations.is_empty());
        active.record("code-review", HashMap::new()).unwrap();

        let loaded = ActiveSkills::load(&path).unwrap();
        assert_eq!(loaded.activations.len(), 1);
        assert_eq!(loaded.activations[0].name, "code-review");
    }
}
