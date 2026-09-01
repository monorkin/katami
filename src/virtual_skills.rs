//! Materializing generated skills into a claude config dir.
//!
//! Skills the reviewer proposed live as rows in the store; before each launch
//! they're rendered into `<config_dir>/skills/agent-<name>/SKILL.md` so the
//! session can invoke them. The sync is one-way and owns only the `agent-`
//! prefix: entries for skills that were archived disappear, everything the
//! user put there themselves is never touched. A skills dir that is itself a
//! symlink is skipped entirely — it points at a directory shared with other
//! setups, and agent doesn't write into shared territory.

use anyhow::Result;
use std::path::Path;

use crate::fsutil;
use crate::memory::Memory;

const PREFIX: &str = "agent-";

pub fn materialize(memory: &Memory, config_dir: &Path) -> Result<()> {
    let skills_dir = config_dir.join("skills");
    if skills_dir.is_symlink() {
        eprintln!(
            "note: {} is a symlink to a shared directory, so generated skills stay unmaterialized",
            skills_dir.display()
        );
        return Ok(());
    }
    std::fs::create_dir_all(&skills_dir)?;

    let skills = memory.generated_skills()?;
    for skill in &skills {
        let contents = format!(
            "---\nname: {PREFIX}{}\ndescription: {}\n---\n\n{}\n",
            skill.name,
            skill.description.replace('\n', " "),
            skill.instructions.trim_end()
        );
        fsutil::write_atomically(
            &skills_dir.join(format!("{PREFIX}{}", skill.name)).join("SKILL.md"),
            &contents,
        )?;
    }

    remove_stale_entries(&skills_dir, &skills)?;
    Ok(())
}

fn remove_stale_entries(
    skills_dir: &Path,
    skills: &[crate::memory::GeneratedSkill],
) -> Result<()> {
    for entry in std::fs::read_dir(skills_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(bare) = name.strip_prefix(PREFIX) else {
            continue;
        };
        if !skills.iter().any(|it| it.name == bare) {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Memory;

    #[test]
    fn materializing_adds_and_removes_only_agent_entries() {
        let config_dir = std::env::temp_dir().join(format!("agent-vskills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config_dir);
        let skills_dir = config_dir.join("skills");
        std::fs::create_dir_all(skills_dir.join("users-own-skill")).unwrap();
        std::fs::create_dir_all(skills_dir.join("agent-stale")).unwrap();

        let memory = Memory::open_in_memory().unwrap();
        memory
            .add_generated_skill("deploy-check", "Verify a deploy", "1. Check the logs.")
            .unwrap();

        materialize(&memory, &config_dir).unwrap();

        let rendered =
            std::fs::read_to_string(skills_dir.join("agent-deploy-check/SKILL.md")).unwrap();
        assert!(rendered.contains("name: agent-deploy-check"));
        assert!(rendered.contains("1. Check the logs."));
        assert!(skills_dir.join("users-own-skill").exists());
        assert!(!skills_dir.join("agent-stale").exists());

        std::fs::remove_dir_all(&config_dir).unwrap();
    }
}
