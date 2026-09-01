//! The curator: the slow loop that keeps the store from becoming a landfill.
//!
//! Sessions and the reviewer only ever add; the curator is the one thing that
//! consolidates and retires. The mechanical parts are pure Rust — archiving
//! generated skills nobody used, re-embedding memories that predate the
//! model, pruning links to titles that no longer exist. The reasoning part —
//! folding an entity's accumulated observations into its card — goes through
//! the same headless `claude -p --model haiku` harness as the reviewer, with
//! the same single trust boundary: strict JSON out, applied in one
//! transaction. Nothing is ever deleted: memories and skills are archived,
//! and an archived row is the backup.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cards;
use crate::embeddings;
use crate::flock;
use crate::fsutil;
use crate::memory::{Kind, Memory, NewObservation};
use crate::paths;

const CONSOLIDATE_AT: usize = 3;

pub struct Thresholds {
    pub archive_skills_after_days: u64,
}

impl Thresholds {
    pub fn load() -> Thresholds {
        let mut thresholds = Thresholds {
            archive_skills_after_days: 90,
        };
        if let Ok(config) = fsutil::read_json(&paths::data_dir().join("config.json"))
            && let Some(days) = config["archive_skills_after_days"].as_u64()
        {
            thresholds.archive_skills_after_days = days;
        }
        thresholds
    }
}

pub fn maybe_spawn(config_dir: &Path) -> Result<()> {
    let memory = Memory::open(&paths::memory_dir())?;
    // At most one automatic run per calendar day; `agent memory curate` is always manual
    if let Some(last_run) = memory.state("curator_last_run")?
        && days_since(&last_run) == 0
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
    let thresholds = Thresholds::load();

    archive_unused_skills(&memory, &thresholds)?;
    reembed_missing(&memory)?;
    consolidate_entities(&memory, config_dir)?;
    cards::render_all(&memory, &paths::memory_dir().join("cards"))?;

    memory.set_state("curator_last_run", &crate::timestamp())?;
    log("curated");
    Ok(())
}

fn archive_unused_skills(memory: &Memory, thresholds: &Thresholds) -> Result<()> {
    for skill in memory.generated_skills()? {
        let last_activity = memory
            .last_used("skill", &format!("agent-{}", skill.name))?
            .unwrap_or(skill.created.clone());
        if days_since(&last_activity) > thresholds.archive_skills_after_days {
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

    let consolidation = ask(&input, config_dir)?;
    let valid_ids: Vec<i64> = observations.iter().map(|it| it.id).collect();
    if consolidation.folded_ids.iter().any(|it| !valid_ids.contains(it)) {
        bail!("the consolidation named observation ids that don't belong to {entity}");
    }

    memory.connection.execute_batch("BEGIN IMMEDIATE")?;
    let applied = apply(memory, entity, card.as_ref().map(|it| it.id), &consolidation);
    match applied {
        Ok(()) => memory.connection.execute_batch("COMMIT")?,
        Err(_) => memory.connection.execute_batch("ROLLBACK")?,
    }
    applied
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
        None => memory.add(&NewObservation {
            kind: Kind::Card,
            entity: Some(entity.to_string()),
            title: card_title(entity),
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

fn card_title(entity: &str) -> String {
    entity.split_once(':').map(|it| it.1).unwrap_or(entity).to_string()
}

fn ask(input: &str, config_dir: &Path) -> Result<Consolidation> {
    let mut command = Command::new("claude");
    command
        .args(["-p", CONSOLIDATE_PROMPT, "--model", "haiku", "--output-format", "json"])
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .env_remove("AGENT_HOOK_SOCKET")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = command
        .spawn()
        .context("could not launch claude — is it on your PATH?")?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("claude exited with {}", output.status);
    }

    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("claude's --output-format json envelope did not parse")?;
    let result = envelope["result"]
        .as_str()
        .context("claude's output carried no result field")?;
    serde_json::from_str(crate::reviewer::strip_fences(result))
        .context("the consolidation was not the expected JSON")
}

fn days_since(timestamp: &str) -> u64 {
    let today = crate::timestamp();
    match (parse_days(timestamp), parse_days(&today)) {
        (Some(then), Some(now)) if now >= then => now - then,
        _ => 0,
    }
}

fn parse_days(timestamp: &str) -> Option<u64> {
    let date = timestamp.get(0..10)?;
    let mut parts = date.split('-');
    let year: u64 = parts.next()?.parse().ok()?;
    let month: u64 = parts.next()?.parse().ok()?;
    let day: u64 = parts.next()?.parse().ok()?;
    Some(year * 372 + month * 31 + day)
}

fn log(message: &str) {
    let path = paths::logs_dir().join("curator.log");
    let _ = std::fs::create_dir_all(paths::logs_dir());
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {message}", crate::timestamp());
    }
}

const CONSOLIDATE_PROMPT: &str = r#"You maintain one entity card in a memory system. Stdin has the card's current body (possibly absent) and a list of dated observations about the same entity, each tagged [id N].

Fold the observations into the card: merge duplicates, keep the newest version of contradicting facts, organize under these markdown sections — Identity, Preferences, Current state, History. Keep it tight; drop nothing that still matters, keep [[links]] that appear.

Reply with ONLY this JSON, no prose:
{"card_body":"the full updated card body in markdown","folded_ids":[1,2]}

folded_ids lists every observation id you fully absorbed into the card. Leave an id out only if the observation should stay standalone."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_math_across_the_iso_timestamps_we_write() {
        assert_eq!(days_since(&crate::timestamp()), 0);
        assert!(days_since("2020-01-01T00:00:00Z") > 365);
        assert_eq!(days_since("not-a-date"), 0);
    }

    #[test]
    fn card_titles_drop_the_entity_prefix() {
        assert_eq!(card_title("project:/home/x/app"), "/home/x/app");
        assert_eq!(card_title("person:Jason"), "Jason");
    }
}
