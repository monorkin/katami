//! `katami memory export` and `import`: memories moving between stores.
//!
//! Export is a read; import is where the judgment lives. An arriving memory
//! either is new here or collides with one already in the store, and a
//! collision is never settled quietly: by default the store's copy wins and
//! the title is reported, `--replace` lets the bundle win, and `--merge` asks
//! the same headless haiku the reviewer uses to write one memory out of the
//! two. Importing the same bundle twice changes nothing.
//!
//! The merges run before anything is written, because each one is seconds of
//! model time and the store's write lock is something live sessions are
//! waiting on. Once every arriving memory has a decision, they're all applied
//! in one transaction — a bundle lands whole or not at all.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::bundle;
use crate::cards;
use crate::clock::timestamp;
use crate::distiller;
use crate::embeddings;
use crate::memory::{Kind, Memory, Portable};
use crate::paths;

#[derive(Debug, PartialEq)]
pub enum Selection {
    All,
    Ids(Vec<i64>),
}

impl Selection {
    pub fn parse(text: &str) -> Result<Selection> {
        if text == "all" {
            Ok(Selection::All)
        } else {
            text.split(',')
                .map(|it| {
                    it.trim().parse::<i64>().with_context(|| {
                        format!("`{it}` is not a memory id — export takes `all`, an id, or ids separated by commas")
                    })
                })
                .collect::<Result<Vec<_>>>()
                .map(Selection::Ids)
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum OnCollision {
    Skip,
    Replace,
    Merge,
}

#[derive(Default, Debug)]
pub struct Outcome {
    pub added: Vec<i64>,
    pub replaced: Vec<i64>,
    pub merged: Vec<i64>,
    pub skipped: Vec<String>,
    pub unchanged: usize,
}

enum Decision {
    Add(Portable),
    Replace(i64, Portable),
    Merge(i64, Portable),
    Skip(String),
    Unchanged,
}

#[derive(Deserialize)]
struct MergedBody {
    body: String,
}

const MERGE_PROMPT: &str = r#"Stdin holds two versions of the same memory from a coding assistant's memory store: the one already in the store, and one arriving from another machine. Write the single memory that should exist afterwards.

Keep every fact either version states that the other doesn't contradict. Where they contradict, the version with the later `updated` timestamp is right. Don't add anything neither version says, don't mention that a merge happened, and keep [[double-bracket links]] exactly as written. Match the length and tone of the originals.

Reply with ONLY this JSON, no prose:
{"body":"the merged memory"}"#;

pub fn export(selection: &str, to: Option<PathBuf>) -> Result<()> {
    let memory = Memory::open(&paths::memory_dir())?;
    let ids = match Selection::parse(selection)? {
        Selection::All => memory.ids()?,
        Selection::Ids(ids) => ids,
    };
    if ids.is_empty() {
        bail!("the store has no memories yet — nothing to export");
    }

    let memories = ids
        .iter()
        .map(|id| Ok((*id, memory.portable(*id)?)))
        .collect::<Result<Vec<_>>>()?;
    let path = destination(to);
    bundle::write(&path, &memories)?;
    println!("Exported {} memories to {}", memories.len(), path.display());
    Ok(())
}

fn destination(to: Option<PathBuf>) -> PathBuf {
    let file_name = format!("katami-memories-{}.zip", &timestamp()[..10]);
    match to {
        Some(path) if path.is_dir() => path.join(file_name),
        Some(path) => path,
        None => PathBuf::from(file_name),
    }
}

pub fn import(path: &Path, on_collision: OnCollision, config_dir: &Path) -> Result<()> {
    let arriving = bundle::read(path)?;
    let memory = Memory::open(&paths::memory_dir())?;
    let outcome = import_into(&memory, arriving, on_collision, config_dir)?;

    for id in outcome.added.iter().chain(&outcome.replaced).chain(&outcome.merged) {
        let stored = memory.get(*id)?;
        if stored.kind != Kind::Status {
            embeddings::embed_into(&memory, *id, &format!("{}\n{}", stored.title, stored.body))?;
        }
        if stored.kind == Kind::Card && !stored.archived {
            cards::render(&stored, &paths::memory_dir().join("cards"))?;
        }
    }

    report(&memory, &outcome)
}

pub fn import_into(
    memory: &Memory,
    arriving: Vec<Portable>,
    on_collision: OnCollision,
    config_dir: &Path,
) -> Result<Outcome> {
    let decisions = arriving
        .into_iter()
        .map(|it| decide(memory, it, on_collision, config_dir))
        .collect::<Result<Vec<_>>>()?;

    memory.with_transaction(|memory| {
        let mut outcome = Outcome::default();
        for decision in decisions {
            match decision {
                Decision::Add(portable) => outcome.added.push(memory.import(&portable)?),
                Decision::Replace(id, portable) => {
                    memory.overwrite(id, &portable)?;
                    outcome.replaced.push(id);
                }
                Decision::Merge(id, portable) => {
                    memory.overwrite(id, &portable)?;
                    outcome.merged.push(id);
                }
                Decision::Skip(title) => outcome.skipped.push(title),
                Decision::Unchanged => outcome.unchanged += 1,
            }
        }
        Ok(outcome)
    })
}

fn decide(memory: &Memory, arriving: Portable, on_collision: OnCollision, config_dir: &Path) -> Result<Decision> {
    let twins = memory
        .twins_of(&arriving)?
        .into_iter()
        .map(|id| Ok((id, memory.portable(id)?)))
        .collect::<Result<Vec<_>>>()?;

    if twins.iter().any(|(_, existing)| same_memory(existing, &arriving)) {
        Ok(Decision::Unchanged)
    } else if arriving.archived {
        // A retired memory is history to keep, never a rival to a live one
        Ok(Decision::Add(arriving))
    } else if let Some((id, existing)) = twins.into_iter().next() {
        settle(id, existing, arriving, on_collision, config_dir)
    } else {
        Ok(Decision::Add(arriving))
    }
}

fn settle(
    id: i64,
    existing: Portable,
    arriving: Portable,
    on_collision: OnCollision,
    config_dir: &Path,
) -> Result<Decision> {
    match on_collision {
        OnCollision::Skip => Ok(Decision::Skip(arriving.title)),
        OnCollision::Replace => Ok(Decision::Replace(id, arriving)),
        OnCollision::Merge => {
            println!("Merging {}…", existing.title);
            let body = merged_body(&existing, &arriving, config_dir)?;
            Ok(Decision::Merge(id, merged(&existing, &arriving, &body)))
        }
    }
}

fn same_memory(existing: &Portable, arriving: &Portable) -> bool {
    let timeless = |it: &Portable| Portable {
        created: String::new(),
        updated: String::new(),
        ..it.clone()
    };
    timeless(existing) == timeless(arriving)
}

fn merged_body(existing: &Portable, arriving: &Portable, config_dir: &Path) -> Result<String> {
    let input = format!(
        "## Already in the store (updated {})\n\n{}\n\n## Arriving (updated {})\n\n{}\n",
        existing.updated,
        existing.body.trim(),
        arriving.updated,
        arriving.body.trim()
    );
    let reply: MergedBody = distiller::ask(MERGE_PROMPT, &input, config_dir, |it: &MergedBody| {
        if it.body.trim().is_empty() {
            bail!("the merged body is empty")
        } else {
            Ok(())
        }
    })
    .with_context(|| format!("could not merge `{}` — rerun with --replace, or without a flag to keep the store's copy", existing.title))?;
    Ok(reply.body.trim().to_string())
}

/// The store's copy keeps its identity — title, entity, creation date — and
/// takes on the merged body. Whatever either side knew beyond the prose
/// survives: every link, a pin from either, and it stays archived only if
/// both had retired it.
pub fn merged(existing: &Portable, arriving: &Portable, body: &str) -> Portable {
    let mut links = existing.links.clone();
    for link in arriving.links.iter().cloned().chain(cards::extract_links(body)) {
        if !links.contains(&link) {
            links.push(link);
        }
    }

    let archived = existing.archived && arriving.archived;
    let archive_reason = if archived { existing.archive_reason.clone() } else { None };

    Portable {
        class: existing.class.clone().or_else(|| arriving.class.clone()),
        body: body.to_string(),
        links,
        pinned: existing.pinned || arriving.pinned,
        archived,
        archive_reason,
        updated: timestamp(),
        ..existing.clone()
    }
}

fn report(memory: &Memory, outcome: &Outcome) -> Result<()> {
    println!(
        "Imported: {} added, {} replaced, {} merged, {} skipped, {} already identical.",
        outcome.added.len(),
        outcome.replaced.len(),
        outcome.merged.len(),
        outcome.skipped.len(),
        outcome.unchanged
    );

    if !outcome.skipped.is_empty() {
        println!("\nAlready here with different content — rerun with --replace or --merge to settle them:");
        for title in &outcome.skipped {
            println!("  {title}");
        }
    }

    let mut homeless: Vec<String> = Vec::new();
    for id in &outcome.added {
        if let Some(entity) = memory.get(*id)?.entity
            && let Some(directory) = entity.strip_prefix("project:")
            && directory.starts_with('/')
            && !Path::new(directory).is_dir()
            && !homeless.contains(&entity)
        {
            homeless.push(entity);
        }
    }
    if !homeless.is_empty() {
        println!("\nThese projects don't exist on this machine, so their memories won't surface until the paths match — fix them with `katami memory edit`, or in the bundle before importing:");
        for entity in &homeless {
            println!("  {entity}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(title: &str, body: &str) -> Portable {
        Portable {
            kind: Kind::Observation,
            class: Some("preference".into()),
            entity: None,
            title: title.into(),
            body: body.into(),
            links: vec![],
            pinned: false,
            archived: false,
            archive_reason: None,
            created: "2026-09-02T12:26:00Z".into(),
            updated: "2026-09-08T06:46:00Z".into(),
        }
    }

    fn card(entity: &str, title: &str, body: &str) -> Portable {
        Portable {
            kind: Kind::Card,
            class: None,
            entity: Some(entity.into()),
            ..observation(title, body)
        }
    }

    fn import_with(memory: &Memory, arriving: Vec<Portable>, on_collision: OnCollision) -> Outcome {
        import_into(memory, arriving, on_collision, Path::new("/nonexistent")).unwrap()
    }

    #[test]
    fn selections_are_all_an_id_or_a_list() {
        assert_eq!(Selection::parse("all").unwrap(), Selection::All);
        assert_eq!(Selection::parse("12").unwrap(), Selection::Ids(vec![12]));
        assert_eq!(Selection::parse("3, 45,7").unwrap(), Selection::Ids(vec![3, 45, 7]));
        assert!(Selection::parse("everything").is_err());
        assert!(Selection::parse("3,,7").is_err());
    }

    #[test]
    fn exported_memories_import_into_an_empty_store_intact() {
        let source = Memory::open_in_memory().unwrap();
        let archived = Portable {
            links: vec!["Commit message style".into()],
            pinned: true,
            archived: true,
            archive_reason: Some("superseded".into()),
            ..observation("Prefers rebase", "Rebase, don't merge.")
        };
        let id = source.import(&archived).unwrap();
        assert_eq!(source.portable(id).unwrap(), archived);

        let destination = Memory::open_in_memory().unwrap();
        let outcome = import_with(&destination, vec![source.portable(id).unwrap()], OnCollision::Skip);
        assert_eq!(outcome.added.len(), 1);
        assert_eq!(destination.portable(outcome.added[0]).unwrap(), archived);
    }

    #[test]
    fn importing_the_same_bundle_twice_changes_nothing() {
        let memory = Memory::open_in_memory().unwrap();
        let arriving = vec![observation("Prefers rebase", "Rebase."), card("person:jason", "Jason", "iOS.")];

        assert_eq!(import_with(&memory, arriving.clone(), OnCollision::Skip).added.len(), 2);

        let again = import_with(&memory, arriving, OnCollision::Replace);
        assert_eq!(again.unchanged, 2);
        assert!(again.added.is_empty() && again.replaced.is_empty());
        assert_eq!(memory.ids().unwrap().len(), 2);
    }

    #[test]
    fn collisions_keep_the_stores_copy_unless_told_to_replace() {
        let memory = Memory::open_in_memory().unwrap();
        let id = memory.import(&observation("Prefers rebase", "Rebase.")).unwrap();
        let card_id = memory.import(&card("person:jason", "Jason", "iOS.")).unwrap();

        let arriving = vec![
            observation("Prefers rebase", "Rebase, and squash fixups."),
            card("person:jason", "Jason F.", "iOS and Android."),
            card("person:david", "David", "Designs."),
        ];

        let skipped = import_with(&memory, arriving.clone(), OnCollision::Skip);
        assert_eq!(skipped.skipped, vec!["Prefers rebase", "Jason F."]);
        assert_eq!(skipped.added.len(), 1);
        assert_eq!(memory.portable(id).unwrap().body, "Rebase.");

        let replaced = import_with(&memory, arriving, OnCollision::Replace);
        assert_eq!(replaced.replaced, vec![id, card_id]);
        assert_eq!(replaced.unchanged, 1);
        assert_eq!(memory.portable(id).unwrap().body, "Rebase, and squash fixups.");
        assert_eq!(memory.portable(card_id).unwrap().title, "Jason F.");
        assert_eq!(memory.ids().unwrap().len(), 3);
    }

    #[test]
    fn retired_versions_travel_without_contesting_the_live_one() {
        let status = |body: &str, archived: bool| Portable {
            kind: Kind::Status,
            class: None,
            entity: Some("project:/home/someone/Work/app".into()),
            archived,
            ..observation("Current state of app", body)
        };
        let bundle = vec![status("PR 3 open.", true), status("PR 7 open.", true), status("PR 12 open.", false)];

        let memory = Memory::open_in_memory().unwrap();
        assert_eq!(import_with(&memory, bundle.clone(), OnCollision::Skip).added.len(), 3);
        assert_eq!(import_with(&memory, bundle, OnCollision::Replace).unchanged, 3);

        let older = vec![status("PR 1 open.", true), status("PR 14 open.", false)];
        let outcome = import_with(&memory, older, OnCollision::Replace);
        assert_eq!(outcome.added.len(), 1);
        assert_eq!(outcome.replaced.len(), 1);

        let live: Vec<String> = memory
            .ids()
            .unwrap()
            .into_iter()
            .map(|id| memory.portable(id).unwrap())
            .filter(|it| !it.archived)
            .map(|it| it.body)
            .collect();
        assert_eq!(live, vec!["PR 14 open."]);
    }

    #[test]
    fn a_bundle_lands_whole_or_not_at_all() {
        let memory = Memory::open_in_memory().unwrap();
        let unstorable = observation("Unstorable", "Body.");
        memory
            .connection
            .execute_batch(
                "CREATE TRIGGER refuse BEFORE INSERT ON memories WHEN new.title = 'Unstorable'
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .unwrap();

        let result = import_into(
            &memory,
            vec![observation("First", "Body."), unstorable],
            OnCollision::Skip,
            Path::new("/nonexistent"),
        );
        assert!(result.is_err());
        assert!(memory.ids().unwrap().is_empty());
    }

    #[test]
    fn merging_keeps_what_either_side_knew() {
        let existing = Portable {
            links: vec!["Commit message style".into()],
            archived: true,
            archive_reason: Some("unused".into()),
            ..observation("Prefers rebase", "Rebase.")
        };
        let arriving = Portable {
            links: vec!["Review workflow".into()],
            pinned: true,
            ..observation("Prefers rebase", "Squash fixups.")
        };

        let merged = merged(&existing, &arriving, "Rebase and squash fixups, see [[Git habits]].");
        assert_eq!(merged.title, "Prefers rebase");
        assert_eq!(merged.body, "Rebase and squash fixups, see [[Git habits]].");
        assert_eq!(merged.links, vec!["Commit message style", "Review workflow", "Git habits"]);
        assert!(merged.pinned);
        assert!(!merged.archived);
        assert_eq!(merged.archive_reason, None);
        assert_eq!(merged.created, existing.created);
    }
}
