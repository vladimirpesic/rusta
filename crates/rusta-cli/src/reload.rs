//! What `/reload` found changed on disk, told to the user and to the model.
//!
//! `/reload` rebuilds the system prompt and the tools from the instruction
//! files, skills and profiles as they are now. The model's earlier turns
//! followed the previous ones, and a rebuilt prompt does not say what is new
//! in it; so the model gets a reminder naming what changed, and the user the
//! same list in the reply.

use std::collections::BTreeMap;
use std::path::PathBuf;

use agent_core::profile::ProfileRegistry;
use agent_core::skills::SkillRegistry;

/// What `/reload` reads, as it was loaded or as it is now.
pub(crate) struct Snapshot<'a> {
    /// The instruction files the system prompt holds, with their text.
    pub instruction_files: &'a [(PathBuf, String)],
    pub skills: &'a SkillRegistry,
    pub profiles: &'a ProfileRegistry,
}

impl Snapshot<'_> {
    fn instruction_files(&self) -> BTreeMap<String, &str> {
        self.instruction_files
            .iter()
            .map(|(path, text)| (path.display().to_string(), text.as_str()))
            .collect()
    }

    fn skills(&self) -> BTreeMap<String, &agent_core::skills::Skill> {
        self.skills
            .list()
            .into_iter()
            .filter_map(|name| Some((name.clone(), self.skills.get(&name)?)))
            .collect()
    }

    fn model_skills(&self) -> BTreeMap<String, &agent_core::skills::Skill> {
        self.skills
            .model_invocable()
            .into_iter()
            .map(|skill| (skill.name.clone(), skill))
            .collect()
    }

    fn profiles(&self) -> BTreeMap<String, &agent_core::profile::Profile> {
        self.profiles
            .names()
            .into_iter()
            .filter_map(|name| Some((name.clone(), self.profiles.get(&name)?)))
            .collect()
    }
}

/// The names added, changed and removed between two snapshots, each sorted.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Changes {
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

impl Changes {
    fn between<T: PartialEq>(before: &BTreeMap<String, T>, after: &BTreeMap<String, T>) -> Self {
        Self {
            added: after
                .keys()
                .filter(|name| !before.contains_key(*name))
                .cloned()
                .collect(),
            changed: after
                .iter()
                .filter(|(name, now)| before.get(*name).is_some_and(|was| was != *now))
                .map(|(name, _)| name.clone())
                .collect(),
            removed: before
                .keys()
                .filter(|name| !after.contains_key(*name))
                .cloned()
                .collect(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    /// `added a, b; changed c; removed d`, leaving out what is empty.
    fn describe(&self) -> String {
        [
            ("added", &self.added),
            ("changed", &self.changed),
            ("removed", &self.removed),
        ]
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
        .map(|(what, names)| format!("{what} {}", names.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
    }
}

/// What changed in each part `/reload` reads.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReloadChanges {
    pub instructions: Changes,
    /// Every skill, as the user runs them with `/skill`.
    pub skills: Changes,
    /// The skills the model may run: those not kept for the user.
    pub model_skills: Changes,
    pub profiles: Changes,
}

impl ReloadChanges {
    pub(crate) fn between(before: &Snapshot<'_>, after: &Snapshot<'_>) -> Self {
        Self {
            instructions: Changes::between(&before.instruction_files(), &after.instruction_files()),
            skills: Changes::between(&before.skills(), &after.skills()),
            model_skills: Changes::between(&before.model_skills(), &after.model_skills()),
            profiles: Changes::between(&before.profiles(), &after.profiles()),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.instructions.is_empty() && self.skills.is_empty() && self.profiles.is_empty()
    }

    /// One line per part that changed, for the user: `Skills: added review`.
    pub(crate) fn summary(&self) -> String {
        lines(&self.instructions, &self.skills, &self.profiles).join("\n")
    }

    /// The reminder that tells the model what changed, or `None` when
    /// nothing it can see did: a skill kept for the user is left out.
    pub(crate) fn reminder(&self) -> Option<String> {
        let lines = lines(&self.instructions, &self.model_skills, &self.profiles);
        if lines.is_empty() {
            return None;
        }
        let lines = lines
            .iter()
            .map(|line| format!("- {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!(
            "The user ran /reload, which read these again from disk; the system prompt and \
             the tools now hold their current versions:\n{lines}\nWhere they differ from what \
             you followed earlier in this conversation, follow the current ones."
        ))
    }
}

/// One line for each of the parts that changed.
fn lines(instructions: &Changes, skills: &Changes, profiles: &Changes) -> Vec<String> {
    [
        ("Instruction files", instructions),
        ("Skills", skills),
        ("Sub-agent profiles", profiles),
    ]
    .into_iter()
    .filter(|(_, changes)| !changes.is_empty())
    .map(|(part, changes)| format!("{part}: {}", changes.describe()))
    .collect()
}

#[cfg(test)]
mod tests {
    use agent_core::profile::Profile;
    use agent_core::skills::{Skill, SkillType};

    use super::*;

    fn skill(name: &str, content: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: None,
            skill_type: SkillType::User,
            when_to_use: None,
            disable_model_invocation: false,
            content: content.to_string(),
        }
    }

    fn skills(list: &[Skill]) -> SkillRegistry {
        let mut registry = SkillRegistry::new();
        for skill in list {
            registry.register(skill.clone());
        }
        registry
    }

    fn files(list: &[(&str, &str)]) -> Vec<(PathBuf, String)> {
        list.iter()
            .map(|(path, text)| (PathBuf::from(path), text.to_string()))
            .collect()
    }

    /// Each part is compared by name and content: what is new, what was
    /// edited, and what went away, and nothing that stayed as it was.
    #[test]
    fn each_part_names_what_was_added_changed_and_removed() {
        let profiles = ProfileRegistry::with_builtin_profiles();
        let mut edited = profiles.clone();
        let mut coder = profiles.get("coder").unwrap().clone();
        coder.system_prompt = Some("Write the tests first.".into());
        edited.register(coder);
        edited.register(Profile {
            name: "reviewer".into(),
            ..Profile::default()
        });

        let before_files = files(&[("/p/AGENTS.md", "Old."), ("/p/a/AGENTS.md", "Same.")]);
        let after_files = files(&[
            ("/p/AGENTS.md", "New."),
            ("/p/a/AGENTS.md", "Same."),
            ("/p/b/AGENTS.md", "Added."),
        ]);
        let mut release = skill("release", "Tag it.");
        release.disable_model_invocation = true;
        let before_skills = skills(&[skill("deploy", "Ship it."), skill("lint", "Lint.")]);
        let after_skills = skills(&[
            skill("deploy", "Ship it."),
            skill("review", "Review."),
            release,
        ]);

        let changes = ReloadChanges::between(
            &Snapshot {
                instruction_files: &before_files,
                skills: &before_skills,
                profiles: &profiles,
            },
            &Snapshot {
                instruction_files: &after_files,
                skills: &after_skills,
                profiles: &edited,
            },
        );
        assert_eq!(
            changes.instructions,
            Changes {
                added: vec!["/p/b/AGENTS.md".into()],
                changed: vec!["/p/AGENTS.md".into()],
                removed: Vec::new(),
            }
        );
        assert_eq!(
            changes.skills,
            Changes {
                added: vec!["release".into(), "review".into()],
                changed: Vec::new(),
                removed: vec!["lint".into()],
            }
        );
        assert_eq!(
            changes.model_skills,
            Changes {
                added: vec!["review".into()],
                changed: Vec::new(),
                removed: vec!["lint".into()],
            }
        );
        assert_eq!(
            changes.profiles,
            Changes {
                added: vec!["reviewer".into()],
                changed: vec!["coder".into()],
                removed: Vec::new(),
            }
        );
        assert_eq!(
            changes.summary(),
            "Instruction files: added /p/b/AGENTS.md; changed /p/AGENTS.md\n\
             Skills: added release, review; removed lint\n\
             Sub-agent profiles: added reviewer; changed coder"
        );
        let reminder = changes.reminder().unwrap();
        assert!(
            reminder.contains("\n- Skills: added review; removed lint\n"),
            "{reminder}"
        );
        assert!(reminder.ends_with("follow the current ones."), "{reminder}");
    }

    /// A change only to skills kept for the user is the user's to hear of:
    /// the model, which cannot run them, is not told.
    #[test]
    fn a_skill_kept_for_the_user_is_not_the_models_news() {
        let instruction_files = files(&[("/p/AGENTS.md", "Rules.")]);
        let profiles = ProfileRegistry::with_builtin_profiles();
        let before = skills(&[]);
        let mut deploy = skill("deploy", "Ship it.");
        deploy.disable_model_invocation = true;
        let after = skills(&[deploy]);
        let changes = ReloadChanges::between(
            &Snapshot {
                instruction_files: &instruction_files,
                skills: &before,
                profiles: &profiles,
            },
            &Snapshot {
                instruction_files: &instruction_files,
                skills: &after,
                profiles: &profiles,
            },
        );
        assert!(!changes.is_empty());
        assert_eq!(changes.summary(), "Skills: added deploy");
        assert_eq!(changes.reminder(), None);
    }

    /// The same files, skills and profiles read again change nothing, and
    /// the model is not told anything.
    #[test]
    fn nothing_changed_gives_no_reminder() {
        let instruction_files = files(&[("/p/AGENTS.md", "Rules.")]);
        let skills = skills(&[skill("deploy", "Ship it.")]);
        let profiles = ProfileRegistry::with_builtin_profiles();
        let snapshot = Snapshot {
            instruction_files: &instruction_files,
            skills: &skills,
            profiles: &profiles,
        };
        let changes = ReloadChanges::between(&snapshot, &snapshot);
        assert!(changes.is_empty());
        assert_eq!(changes.summary(), "");
        assert_eq!(changes.reminder(), None);
    }
}
