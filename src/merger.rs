//! Turning two versions of a memory back into one.
//!
//! When two machines change the same memory at once, sync keeps both — the
//! local one in place, the other beside it as a sibling — because no rule can
//! know which sentence the person would rather lose. What a rule can't do a
//! model can: haiku reads both and writes the memory that says what each of
//! them said. That takes seconds and a working `claude`, so it never happens
//! on a hook or inside a sync; it runs where the reviewer and curator already
//! run, after a session, and a conflict it can't settle today just waits.
//!
//! The merged memory is an ordinary write by this node that descends from
//! both versions, so it reaches the other machines as plain news. If another
//! machine merged the same pair in the meantime, the store recognizes the two
//! merges as the same one and keeps either.

use anyhow::Result;
use std::path::Path;

use crate::flock;
use crate::logs;
use crate::memory::Memory;
use crate::paths;
use crate::transfer;

pub fn run(config_dir: &Path) -> Result<()> {
    let Some(_lock) = flock::try_acquire(&paths::memory_dir().join("merger.lock"))? else {
        return Ok(());
    };
    merge_conflicts(&Memory::open(&paths::memory_dir())?, config_dir)
}

/// Siblings are merged one at a time into whatever the local version has
/// become, so three machines editing at once still end as one memory.
pub fn merge_conflicts(memory: &Memory, config_dir: &Path) -> Result<()> {
    for id in memory.conflicted_ids()? {
        for sibling in memory.siblings_of(id)? {
            let local = memory.portable(id)?;
            match transfer::merged_body(&local, &sibling.memory, config_dir) {
                Ok(body) => {
                    memory.resolve_sibling(&transfer::merged(&local, &sibling.memory, &body), &sibling)?;
                    transfer::refresh_derived(memory, &[id])?;
                    log(&format!("merged two versions of [[{}]]", local.title));
                }
                Err(error) => log(&format!("could not merge [[{}]] yet: {error:#}", local.title)),
            }
        }
    }
    Ok(())
}

fn log(message: &str) {
    if cfg!(not(test)) {
        logs::append("merger", message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Kind, NewMemory};

    fn pull(into: &Memory, from: &Memory) {
        let delta = from.delta_for(&into.knowledge().unwrap()).unwrap();
        into.absorb_delta(&delta).unwrap();
    }

    #[test]
    fn versions_that_only_disagree_beyond_the_prose_merge_without_a_model() {
        let (mini, laptop) = (Memory::open_in_memory().unwrap(), Memory::open_in_memory().unwrap());
        let id = mini
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: Some("project:/home/someone/app".into()),
                title: "Deploy on Fridays".into(),
                body: "We deploy on Fridays.".into(),
                links: vec!["Deploys".into()],
                source_session: None,
                class: None,
            })
            .unwrap();
        pull(&laptop, &mini);

        mini.replace_links(id, &["Deploys".to_string(), "Release checklist".to_string()]).unwrap();
        let renamed = crate::memory::Portable {
            entity: Some("project:example.com/acme/app".into()),
            pinned: true,
            ..laptop.portable(id).unwrap()
        };
        laptop.overwrite(id, &renamed).unwrap();
        pull(&laptop, &mini);
        assert_eq!(laptop.conflicted_ids().unwrap(), vec![id]);

        merge_conflicts(&laptop, Path::new("/nonexistent")).unwrap();

        let merged = laptop.portable(id).unwrap();
        assert!(laptop.conflicted_ids().unwrap().is_empty());
        assert_eq!(merged.body, "We deploy on Fridays.");
        assert_eq!(merged.links, vec!["Deploys", "Release checklist"]);
        assert_eq!(merged.entity.as_deref(), Some("project:example.com/acme/app"));
        assert!(merged.pinned);

        pull(&mini, &laptop);
        assert_eq!(mini.portable(id).unwrap(), merged);
        assert!(mini.conflicted_ids().unwrap().is_empty());
    }
}
