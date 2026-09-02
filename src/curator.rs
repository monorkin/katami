//! The curator: the slow loop that keeps the store from becoming a landfill.
//!
//! Sessions and the reviewer only ever add; the curator is the one thing that
//! consolidates and archives. The mechanical parts are pure Rust — archiving
//! generated skills nobody used, re-embedding memories that predate the
//! model, pruning links to titles that no longer exist. The reasoning part —
//! folding an entity's accumulated observations into its card — goes through
//! the same headless `claude -p --model haiku` harness as the reviewer, with
//! the same single trust boundary: strict JSON out, applied in one
//! transaction. Nothing is ever deleted: memories and skills are archived,
//! and an archived row is the backup.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cards;
use crate::clock;
use crate::distiller;
use crate::embeddings;
use crate::flock;
use crate::fsutil;
use crate::logs;
use crate::memory::{Kind, Memory, NewMemory};
use crate::paths;

const CONSOLIDATE_AT: usize = 3;

fn archive_skills_after_days() -> u64 {
    fsutil::read_json(&paths::data_dir().join("config.json"))
        .ok()
        .and_then(|it| it["archive_skills_after_days"].as_u64())
        .unwrap_or(90)
}

pub fn maybe_spawn(config_dir: &Path) -> Result<()> {
    let memory = Memory::open(&paths::memory_dir())?;
    // At most one automatic run per calendar day; `agent memory curate` is always manual
    if let Some(last_run) = memory.state("curator_last_run")?
        && clock::days_since(&last_run) == 0
    {
        return Ok(());
    }

    let agent = std::env::current_exe().context("could not determine the agent binary path")?;
    let mut command = Command::new(agent);
    command
        .args(["curate", "--config-dir"])
        .arg(config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().context("could not spawn the curator")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn run(config_dir: &Path) -> Result<()> {
    let Some(_lock) = flock::try_acquire(&paths::memory_dir().join("curator.lock"))? else {
        return Ok(());
    };
    let memory = Memory::open(&paths::memory_dir())?;

    archive_unused_skills(&memory)?;
    archive_stale_statuses(&memory)?;
    archive_never_retrieved(&memory)?;
    reembed_missing(&memory)?;
    consolidate_entities(&memory, config_dir)?;
    cards::render_all(&memory, &paths::memory_dir().join("cards"))?;
    sweep_files(&memory)?;

    memory.set_state("curator_last_run", &clock::timestamp())?;
    log("curated");
    Ok(())
}

/// A status snapshot nobody refreshed in two weeks is stale by definition —
/// better silence than confidently wrong context.
fn archive_stale_statuses(memory: &Memory) -> Result<()> {
    for status in memory.list()?.iter().filter(|it| it.kind == Kind::Status) {
        if clock::days_since(&status.updated) > 14 {
            memory.archive(status.id)?;
            log(&format!("archived stale status for {}", status.entity.as_deref().unwrap_or("?")));
        }
    }
    Ok(())
}

fn archive_never_retrieved(memory: &Memory) -> Result<()> {
    for observation in memory.unretrieved_observations()? {
        if clock::days_since(&observation.updated) > 90 {
            memory.archive(observation.id)?;
            log(&format!("archived never-retrieved [[{}]]", observation.title));
        }
    }
    Ok(())
}

/// The append-only leftovers: per-pid supervisor logs, overlays leaked by
/// crashed launches, cursors for transcripts that no longer exist, and usage
/// rows old enough to carry no signal.
fn sweep_files(memory: &Memory) -> Result<()> {
    remove_older_than(&paths::logs_dir(), "supervisor-", 30)?;
    remove_older_than(&paths::overlays_dir(), "", 7)?;
    memory.prune_cursors(|path| std::path::Path::new(path).exists())?;
    memory.connection.execute(
        "DELETE FROM usage WHERE used_at < ?1",
        [clock::timestamp_days_ago(180)],
    )?;
    Ok(())
}

fn remove_older_than(directory: &Path, prefix: &str, days: u64) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(prefix) {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|it| it.modified())
            .ok()
            .and_then(|it| it.elapsed().ok());
        if age.is_some_and(|it| it.as_secs() > days * 86_400) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn archive_unused_skills(memory: &Memory) -> Result<()> {
    let archive_after = archive_skills_after_days();
    for skill in memory.generated_skills()? {
        let last_activity = memory
            .last_used("skill", &format!("agent-{}", skill.name))?
            .unwrap_or(skill.created.clone());
        if clock::days_since(&last_activity) > archive_after {
            memory.archive_generated_skill(&skill.name)?;
            log(&format!("archived unused skill {}", skill.name));
        }
    }
    Ok(())
}

fn reembed_missing(memory: &Memory) -> Result<()> {
    for (id, text) in memory.unembedded(embeddings::MODEL_NAME)? {
        embeddings::embed_into(memory, id, &text)?;
    }
    Ok(())
}

fn consolidate_entities(memory: &Memory, config_dir: &Path) -> Result<()> {
    for entity in memory.entities_with_observations(CONSOLIDATE_AT)? {
        if let Err(error) = consolidate(memory, config_dir, &entity) {
            log(&format!("consolidating {entity} failed: {error:#}"));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct Consolidation {
    card_body: String,
    folded_ids: Vec<i64>,
}

fn consolidate(memory: &Memory, config_dir: &Path, entity: &str) -> Result<()> {
    let observations = memory.observations_for_entity(entity)?;
    let card = memory.card_for_entity(entity)?;

    let mut input = String::new();
    if let Some(card) = &card {
        input.push_str(&format!("Current card:\n{}\n\n", card.body));
    }
    input.push_str("Observations:\n");
    for observation in &observations {
        input.push_str(&format!(
            "[id {}] {}: {}\n",
            observation.id, observation.title, observation.body
        ));
    }

    let consolidation: Consolidation = distiller::ask(CONSOLIDATE_PROMPT, &input, config_dir)?;
    let valid_ids: Vec<i64> = observations.iter().map(|it| it.id).collect();
    if consolidation.folded_ids.iter().any(|it| !valid_ids.contains(it)) {
        bail!("the consolidation named observation ids that don't belong to {entity}");
    }

    memory.with_transaction(|memory| {
        apply(memory, entity, card.as_ref().map(|it| it.id), &consolidation)
    })
}

fn apply(
    memory: &Memory,
    entity: &str,
    card_id: Option<i64>,
    consolidation: &Consolidation,
) -> Result<()> {
    let id = match card_id {
        Some(id) => {
            memory.update_body(id, &consolidation.card_body)?;
            id
        }
        None => memory.add(&NewMemory {
            kind: Kind::Card,
            entity: Some(entity.to_string()),
            title: cards::entity_name(entity).to_string(),
            body: consolidation.card_body.clone(),
            links: cards::extract_links(&consolidation.card_body),
            source_session: None,
        })?,
    };
    embeddings::embed_into(memory, id, &consolidation.card_body)?;

    for folded in &consolidation.folded_ids {
        memory.archive(*folded)?;
    }
    Ok(())
}

fn log(message: &str) {
    logs::append("curator", message);
}

const CONSOLIDATE_PROMPT: &str = r#"You maintain one entity card in a memory system. Stdin has the card's current body (possibly absent) and a list of dated observations about the same entity, each tagged [id N].

Fold the observations into the card: merge duplicates, keep the newest version of contradicting facts, organize under these markdown sections — Identity, Preferences, History. Cards hold only durable material (they can be injected into any session); leave in-flight work status out entirely. Keep it tight; drop nothing that still matters, keep [[links]] that appear.

Reply with ONLY this JSON, no prose:
{"card_body":"the full updated card body in markdown","folded_ids":[1,2]}

folded_ids lists every observation id you fully absorbed into the card. Leave an id out only if the observation should stay standalone."#;

