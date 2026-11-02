//! Skill registry: discovery and lookup of Markdown skill files.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use super::{Skill, parser::parse_skill};
use crate::errors::AgentError;

/// Registry of loaded skills keyed by skill name.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Register a skill, overwriting any existing skill with the same name.
    pub fn register(&mut self, skill: Skill) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Load all `.md` files from a directory into the registry.
    ///
    /// Missing directories are silently ignored. Files are processed in sorted
    /// order so behaviour is deterministic.
    pub fn load_dir(&mut self, dir: impl AsRef<Path>) -> Result<(), AgentError> {
        let dir = dir.as_ref();
        if !dir.exists() {
            return Ok(());
        }
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| AgentError::Config(format!("failed to read skill dir {dir:?}: {e}")))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.eq_ignore_ascii_case("md"))
                    .unwrap_or(false)
            })
            .collect();
        entries.sort();

        for path in entries {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| AgentError::Config(format!("failed to read skill {path:?}: {e}")))?;
            let name = path.file_stem().map(|s| s.to_string_lossy().to_string());
            let skill = parse_skill(name.as_deref(), &content)
                .map_err(|e| AgentError::Config(format!("failed to parse skill {path:?}: {e}")))?;
            self.register(skill);
        }
        Ok(())
    }

    /// Load skills from multiple directories, with later directories taking
    /// precedence over earlier ones.
    pub fn load_dirs(dirs: &[PathBuf]) -> Result<Self, AgentError> {
        let mut registry = Self::new();
        for dir in dirs {
            registry.load_dir(dir)?;
        }
        Ok(registry)
    }

    /// Return a reference to a skill by name.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    /// The skills the model may run, by name: all but those marked
    /// `disable_model_invocation`, which only the user runs, with `/skill`.
    pub fn model_invocable(&self) -> Vec<&Skill> {
        let mut skills: Vec<&Skill> = self
            .skills
            .values()
            .filter(|skill| !skill.disable_model_invocation)
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    /// Return the names of all registered skills.
    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.skills.keys().cloned().collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_load_dir_registers_skills() {
        let temp = tempfile::tempdir().unwrap();
        let mut file = std::fs::File::create(temp.path().join("review.md")).unwrap();
        file.write_all(b"---\nname: review\n---\nReview this.")
            .unwrap();
        drop(file);

        let registry = SkillRegistry::load_dirs(&[temp.path().to_path_buf()]).unwrap();
        assert!(registry.get("review").is_some());
        let names = registry.list();
        assert_eq!(names, vec!["review".to_string()]);
    }

    #[test]
    fn test_later_dir_overrides_earlier() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let mut f = std::fs::File::create(first.path().join("shared.md")).unwrap();
        f.write_all(b"---\nname: shared\n---\nfirst").unwrap();
        drop(f);
        let mut f = std::fs::File::create(second.path().join("shared.md")).unwrap();
        f.write_all(b"---\nname: shared\n---\nsecond").unwrap();
        drop(f);

        let registry =
            SkillRegistry::load_dirs(&[first.path().to_path_buf(), second.path().to_path_buf()])
                .unwrap();
        assert_eq!(registry.get("shared").unwrap().content, "second");
    }

    #[test]
    fn test_missing_dir_is_ignored() {
        let registry =
            SkillRegistry::load_dirs(&[PathBuf::from("/does/not/exist/skills")]).unwrap();
        assert!(registry.list().is_empty());
    }
}
