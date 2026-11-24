//! The profiles and skills a session loads, from the user, the workspace and
//! the directories the config names.

use agent_core::{ProfileRegistry, SkillRegistry};

use crate::config;

/// Load user and project profiles into a registry.
pub(crate) fn load_profile_registry(cwd: &std::path::Path) -> anyhow::Result<ProfileRegistry> {
    let mut dirs = Vec::new();
    if let Some(config_dir) = agent_core::user_dirs::config_dir() {
        dirs.push(config_dir.join("rusta").join("profiles"));
    }
    dirs.push(cwd.join(".rusta").join("profiles"));
    ProfileRegistry::load_dirs(&dirs)
        .map_err(|e| anyhow::anyhow!("failed to load profile registry: {e}"))
}

/// Load user, shared, and project skills into a registry: the user's, those
/// in `~/.agents`, the `.rusta/skills` of each of the workspace's additional
/// directories, the working directory's, and `extra_dirs`; of two skills of
/// one name, the later wins, so the working directory's outranks an
/// additional directory's.
pub(crate) fn load_skill_registry(
    cwd: &std::path::Path,
    additional_dirs: &[std::path::PathBuf],
    extra_dirs: &[std::path::PathBuf],
) -> anyhow::Result<SkillRegistry> {
    SkillRegistry::load_dirs(&skill_dirs(
        agent_core::user_dirs::config_dir(),
        dirs::home_dir(),
        cwd,
        additional_dirs,
        extra_dirs,
    ))
    .map_err(|e| anyhow::anyhow!("failed to load skill registry: {e}"))
}

/// The directories [`load_skill_registry`] reads, lowest precedence first.
fn skill_dirs(
    config_dir: Option<std::path::PathBuf>,
    home: Option<std::path::PathBuf>,
    cwd: &std::path::Path,
    additional_dirs: &[std::path::PathBuf],
    extra_dirs: &[std::path::PathBuf],
) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(config_dir) = config_dir {
        dirs.push(config_dir.join("rusta").join("skills"));
    }
    if let Some(home) = home {
        dirs.push(home.join(".agents").join("skills"));
    }
    for dir in additional_dirs {
        dirs.push(dir.join(".rusta").join("skills"));
    }
    dirs.push(cwd.join(".rusta").join("skills"));
    for dir in extra_dirs {
        dirs.push(config::expand_tilde(dir));
    }
    dirs
}

#[cfg(test)]
mod skill_dir_tests {
    /// Skills come from each additional directory too, below the working
    /// directory's own, whose skill of the same name wins.
    #[test]
    fn skills_of_additional_directories_rank_below_the_workspaces() {
        let root = tempfile::tempdir().unwrap();
        let (cwd, extra) = (root.path().join("app"), root.path().join("lib"));
        for (dir, body) in [(&cwd, "the app's"), (&extra, "the lib's")] {
            let skills = dir.join(".rusta").join("skills");
            std::fs::create_dir_all(&skills).unwrap();
            std::fs::write(skills.join("review.md"), body).unwrap();
        }
        std::fs::write(extra.join(".rusta/skills/release.md"), "ship it").unwrap();
        assert_eq!(
            super::skill_dirs(None, None, &cwd, std::slice::from_ref(&extra), &[]),
            [extra.join(".rusta/skills"), cwd.join(".rusta/skills")]
        );
        let registry = agent_core::skills::SkillRegistry::load_dirs(&super::skill_dirs(
            None,
            None,
            &cwd,
            &[extra],
            &[],
        ))
        .unwrap();
        assert_eq!(registry.get("review").unwrap().content.trim(), "the app's");
        assert!(registry.get("release").is_some());
    }
}
