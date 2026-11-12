//! Parser for Markdown skill files with YAML frontmatter.

use std::collections::HashMap;

use serde::Deserialize;

use super::{Skill, SkillType};
use crate::errors::AgentError;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SkillFrontmatter {
    #[serde(deserialize_with = "crate::yaml::or_default")]
    name: String,
    description: Option<String>,
    #[serde(rename = "type")]
    skill_type: Option<String>,
    when_to_use: Option<String>,
    disable_model_invocation: Option<bool>,
}

/// Parse a skill from its raw Markdown source.
///
/// Supports leading YAML frontmatter delimited by `---` lines. If no frontmatter
/// is present, the entire source is treated as the skill body and the name must
/// be supplied by the caller (e.g. derived from the filename).
pub fn parse_skill(name: Option<&str>, source: &str) -> Result<Skill, AgentError> {
    let trimmed = source.trim_start();
    let (frontmatter, body) = if let Some(after_open) = trimmed.strip_prefix("---") {
        match after_open.find("---") {
            Some(idx) => {
                let raw = &after_open[..idx];
                let yaml = raw.trim();
                // The lines before the YAML, so an error names the file's line.
                let start = source.len() - after_open.len() + (raw.len() - raw.trim_start().len());
                let lines_before = source[..start].matches('\n').count() as u64;
                let body = after_open[idx + 3..].trim_start().to_string();
                (Some((yaml, lines_before)), body)
            }
            None => {
                return Err(AgentError::Config(
                    "skill frontmatter is missing closing ---".to_string(),
                ));
            }
        }
    } else {
        (None, trimmed.to_string())
    };

    let meta: SkillFrontmatter = match frontmatter {
        Some((yaml, lines_before)) => crate::yaml::from_str(yaml, lines_before)
            .map_err(|e| AgentError::Config(format!("skill YAML frontmatter: {e}")))?,
        None => SkillFrontmatter::default(),
    };

    let name = if meta.name.is_empty() {
        name.map(ToString::to_string)
            .ok_or_else(|| AgentError::Config("skill is missing a name".to_string()))?
    } else {
        meta.name
    };

    let skill_type = meta
        .skill_type
        .as_deref()
        .map(parse_skill_type)
        .unwrap_or(SkillType::System);

    Ok(Skill {
        name,
        description: meta.description,
        skill_type,
        when_to_use: meta.when_to_use,
        disable_model_invocation: meta.disable_model_invocation.unwrap_or(false),
        content: body,
    })
}

fn parse_skill_type(s: &str) -> SkillType {
    match s.to_lowercase().as_str() {
        "user" => SkillType::User,
        _ => SkillType::System,
    }
}

/// Parse `key=value` arguments from a list of tokens.
pub fn parse_args(tokens: &[&str]) -> HashMap<String, String> {
    let mut args = HashMap::new();
    for token in tokens {
        if let Some((key, value)) = token.split_once('=') {
            args.insert(key.to_string(), value.to_string());
        }
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_frontmatter() {
        let source = r#"---
name: code-review
description: Review code for issues
type: user
when_to_use: When the user shares a diff
disable_model_invocation: true
---
Review the following code carefully.

Look for bugs and style issues.
"#;
        let skill = parse_skill(None, source).unwrap();
        assert_eq!(skill.name, "code-review");
        assert_eq!(skill.description.as_deref(), Some("Review code for issues"));
        assert_eq!(skill.skill_type, SkillType::User);
        assert_eq!(
            skill.when_to_use.as_deref(),
            Some("When the user shares a diff")
        );
        assert!(skill.disable_model_invocation);
        assert!(
            skill
                .content
                .contains("Review the following code carefully.")
        );
    }

    #[test]
    fn test_parse_defaults() {
        let source = "---\nname: default-skill\n---\nBody text.";
        let skill = parse_skill(None, source).unwrap();
        assert_eq!(skill.name, "default-skill");
        assert_eq!(skill.skill_type, SkillType::System);
        assert!(!skill.disable_model_invocation);
        assert_eq!(skill.content, "Body text.");
    }

    #[test]
    fn test_parse_missing_name_uses_filename() {
        let source = "Plain body without frontmatter.";
        let skill = parse_skill(Some("plain"), source).unwrap();
        assert_eq!(skill.name, "plain");
        assert_eq!(skill.content, "Plain body without frontmatter.");
    }

    #[test]
    fn test_parse_missing_name_errors() {
        let source = "---\ndescription: no name\n---\nBody";
        assert!(parse_skill(None, source).is_err());
    }

    /// A `name` left without a value is no name: the file's name is used.
    #[test]
    fn a_blank_name_uses_the_filename() {
        let source = "---\nname:\ndescription: d\n---\nBody";
        let skill = parse_skill(Some("from-file"), source).unwrap();
        assert_eq!(skill.name, "from-file");
        assert_eq!(skill.description.as_deref(), Some("d"));
    }

    #[test]
    fn test_parse_invalid_yaml_errors() {
        let source = "---\nname: [\n---\nBody";
        assert!(parse_skill(None, source).is_err());
    }

    /// An error in the frontmatter names the line in the whole file.
    #[test]
    fn a_frontmatter_error_names_the_files_line() {
        let source = "\n---\nname: s\ndisable_model_invocation: yes\n---\nBody";
        let error = parse_skill(None, source).unwrap_err().to_string();
        assert!(error.contains("line 4, column"), "{error}");
        assert!(
            error.ends_with("`disable_model_invocation: yes`"),
            "{error}"
        );
    }

    #[test]
    fn test_parse_unclosed_frontmatter_errors() {
        let source = "---\nname: x\nBody";
        assert!(parse_skill(None, source).is_err());
    }

    #[test]
    fn test_parse_args() {
        let args = parse_args(&["foo=bar", "baz=qux", "noseparator"]);
        assert_eq!(args.get("foo").map(String::as_str), Some("bar"));
        assert_eq!(args.get("baz").map(String::as_str), Some("qux"));
        assert!(!args.contains_key("noseparator"));
    }
}
