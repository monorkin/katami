//! The reviewer: a cheap background pass that turns conversations into
//! memories.
//!
//! After a session stops, a detached `agent review` process feeds the
//! transcript delta — user messages and final assistant texts only — to a
//! headless `claude -p --model haiku` run and asks two questions: did the
//! user reveal anything worth remembering, and is there a repeatable
//! workflow worth a skill? The reviewer's stdout is the only trust boundary:
//! agent validates the JSON and applies it in one transaction, so a confused
//! model can't half-write the store. It runs with the session's own
//! CLAUDE_CONFIG_DIR so the right account's auth pays for it, and with the
//! hook socket stripped so its own hooks no-op.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cards;
use crate::distiller;
use crate::embeddings;
use crate::flock;
use crate::logs;
use crate::memory::{Kind, Memory, NewMemory};
use crate::paths;
use crate::transcript;

const DEBOUNCE_USER_TURNS: usize = 5;
const FAILURE_CAP: u32 = 2;

#[derive(Deserialize)]
struct Review {
    #[serde(default)]
    observations: Vec<Observation>,
    #[serde(default)]
    skill_proposals: Vec<SkillProposal>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
struct Observation {
    title: String,
    body: String,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    links: Vec<String>,
    #[serde(default)]
    supersedes: Vec<i64>,
}

#[derive(Deserialize)]
struct SkillProposal {
    name: String,
    description: String,
    instructions: String,
}

/// Called on Stop: spawns a detached review once enough of the conversation
/// has accumulated — the hook must return within its timeout, so the review
/// itself never runs inline.
pub fn maybe_spawn(transcript_path: &str, config_dir: &Path, cwd: Option<&str>) -> Result<()> {
    let turns = unreviewed_turns(transcript_path)?;
    if transcript::user_turns(&turns) >= DEBOUNCE_USER_TURNS {
        spawn_detached(transcript_path, config_dir, cwd)?;
    }
    Ok(())
}

/// Called on SessionEnd: the transcript will never fire another event, so
/// anything unreviewed gets reviewed now, debounce or no debounce.
pub fn spawn_final_review(transcript_path: &str, config_dir: &Path, cwd: Option<&str>) -> Result<()> {
    if !unreviewed_turns(transcript_path)?.is_empty() {
        spawn_detached(transcript_path, config_dir, cwd)?;
    }
    Ok(())
}

fn unreviewed_turns(transcript_path: &str) -> Result<Vec<transcript::Turn>> {
    let memory = Memory::open(&paths::memory_dir())?;
    let offset = memory.cursor(transcript_path)?;
    let (turns, _) = transcript::delta_since(Path::new(transcript_path), offset)?;
    Ok(turns)
}

fn spawn_detached(transcript_path: &str, config_dir: &Path, cwd: Option<&str>) -> Result<()> {
    let agent = std::env::current_exe().context("could not determine the agent binary path")?;
    let mut command = Command::new(agent);
    command
        .args(["review", "--transcript", transcript_path, "--config-dir"])
        .arg(config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.args(["--cwd", cwd]);
    }
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().context("could not spawn the reviewer")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn run(transcript_path: &Path, config_dir: &Path, cwd: Option<&Path>) -> Result<()> {
    // Waiting, not skipping: this may be the last event this transcript ever
    // fires, and losing the race to another reviewer must not lose the review
    let lock_patience = std::time::Duration::from_secs(300);
    let Some(_lock) = flock::acquire(&paths::memory_dir().join("reviewer.lock"), lock_patience)?
    else {
        log("gave up waiting for the reviewer lock");
        return Ok(());
    };

    let memory = Memory::open(&paths::memory_dir())?;
    let transcript_key = transcript_path.display().to_string();
    let offset = memory.cursor(&transcript_key)?;
    let (turns, new_offset) = transcript::delta_since(transcript_path, offset)?;
    if turns.is_empty() {
        return Ok(());
    }

    let failure_key = format!("reviewer_failures:{transcript_key}:{offset}");
    let failures: u32 = memory
        .state(&failure_key)?
        .and_then(|it| it.parse().ok())
        .unwrap_or(0);

    let entity = cwd.map(|it| cards::project_entity(it));
    let input = format!(
        "{}\nTranscript:\n{}",
        known_context(&memory, entity.as_deref())?,
        transcript::distill(&turns)
    );
    let source_session = transcript_path
        .file_stem()
        .map(|it| it.to_string_lossy().to_string());
    let reviewed = distiller::ask::<Review>(&review_prompt(cwd), &input, config_dir)
        .and_then(|review| {
            apply(&memory, &review, entity.as_deref(), source_session.as_deref())?;
            Ok(review)
        });
    match reviewed {
        Ok(review) => {
            memory.set_cursor(&transcript_key, new_offset)?;
            memory.clear_state(&failure_key)?;
            for observation in &review.observations {
                log(&format!("observed [[{}]]", observation.title));
            }
            for proposal in &review.skill_proposals {
                log(&format!("proposed skill {}", proposal.name));
            }
            log(&format!("reviewed {transcript_key} through byte {new_offset}"));
            Ok(())
        }
        Err(error) if failures + 1 >= FAILURE_CAP => {
            // Giving up: advance anyway so one bad delta can't block reviews forever
            memory.set_cursor(&transcript_key, new_offset)?;
            memory.clear_state(&failure_key)?;
            log(&format!("giving up on {transcript_key} at byte {offset}: {error:#}"));
            Ok(())
        }
        Err(error) => {
            memory.set_state(&failure_key, &(failures + 1).to_string())?;
            log(&format!("review of {transcript_key} failed, will retry: {error:#}"));
            Ok(())
        }
    }
}

/// What the store already knows, prepended to the reviewer's input so it can
/// supersede and skip instead of restating — a reviewer that writes blind
/// duplicates the store back into itself.
fn known_context(memory: &Memory, entity: Option<&str>) -> Result<String> {
    let mut known = String::from(
        "What you already know (do not restate; if the conversation updates or contradicts an item, supersede it by id):\n",
    );
    if let Some(entity) = entity {
        if let Some(card) = memory.card_for_entity(entity)? {
            known.push_str(&format!("Card for {entity}:\n{}\n", card.body.trim()));
        }
        if let Some(status) = memory.status_for_entity(entity)? {
            known.push_str(&format!("Current status (as of {}):\n{}\n", &status.updated[..10], status.body.trim()));
        }
        for observation in memory.recent_observations_for_entity(entity, 15)? {
            known.push_str(&format!("[id {}] {}: {}\n", observation.id, observation.title, observation.body));
        }
    }
    let entities = memory.entities()?;
    if !entities.is_empty() {
        known.push_str(&format!("Known entities: {}\n", entities.join(", ")));
    }
    Ok(known)
}

fn apply(
    memory: &Memory,
    review: &Review,
    entity: Option<&str>,
    source_session: Option<&str>,
) -> Result<()> {
    memory.with_transaction(|memory| {
        for observation in &review.observations {
            let mut links = cards::extract_links(&observation.body);
            for link in &observation.links {
                if !links.contains(link) {
                    links.push(link.clone());
                }
            }
            let title = single_line(&observation.title);
            let id = memory.add(&NewMemory {
                kind: Kind::Observation,
                entity: observation.entity.clone(),
                title: title.clone(),
                body: observation.body.clone(),
                links,
                source_session: source_session.map(str::to_string),
            })?;
            embeddings::embed_into(memory, id, &format!("{title}\n{}", observation.body))?;
            supersede(memory, &observation.supersedes)?;
        }

        for proposal in &review.skill_proposals {
            if !valid_skill_name(&proposal.name) {
                anyhow::bail!(
                    "the review proposed a skill named '{}' — names must be lowercase letters, digits, and dashes",
                    proposal.name
                );
            }
            memory.add_generated_skill(&proposal.name, &proposal.description, &proposal.instructions)?;
        }

        if let (Some(entity), Some(status)) = (entity, &review.status) {
            if !status.trim().is_empty() {
                memory.upsert_status(entity, status)?;
            }
        }
        Ok(())
    })
}

fn supersede(memory: &Memory, ids: &[i64]) -> Result<()> {
    for id in ids {
        let stored = memory
            .get(*id)
            .with_context(|| format!("the review superseded id {id}, which doesn't exist"))?;
        if stored.kind != Kind::Observation || stored.pinned {
            anyhow::bail!(
                "the review tried to supersede id {id}, but only unpinned observations can be superseded"
            );
        }
        memory.archive(*id)?;
    }
    Ok(())
}

/// Skill names become directory names under the claude config dir — anything
/// but this strict shape is a path traversal waiting to happen.
fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().next().is_some_and(|it| it.is_ascii_lowercase() || it.is_ascii_digit())
        && name.chars().all(|it| it.is_ascii_lowercase() || it.is_ascii_digit() || it == '-')
}

fn single_line(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn log(message: &str) {
    logs::append("reviewer", message);
}

fn review_prompt(cwd: Option<&Path>) -> String {
    let project_rule = match cwd {
        Some(cwd) => format!(
            "- A fact about the project you're working in: \"project:{}\"\n  (this session's project — copy the string exactly, never invent a shorter name for it)",
            cwd.display()
        ),
        None => "- A fact about a specific project: omit entity — this session's project is unknown".to_string(),
    };

    format!(
        r#"You are reviewing a conversation between a user and their coding assistant. Stdin has two parts: what the memory store already knows, then the conversation transcript.

Extract three things:
1. Observations: durable facts that will still be true in a month — the user's preferences, corrections they gave, facts about themselves, their projects, and the people they work with. NOT the state of in-progress work, and NOT anything the store already knows. If the conversation updates or contradicts a known [id N] item, put that id in the new observation's "supersedes" list. An observation's entity is what the fact is ABOUT, not where it was discussed — a fact about a tool, a service, or general practice gets no entity even when it came up inside a project. Skip anything derivable from the code itself, and skip discussion about this memory system.
2. Status: what's in flight in this session's project right now — open PRs, half-done migrations, blocked work. This REPLACES the previous status entirely, so restate anything still in flight. Omit the field if nothing is in flight or the project is unknown.
3. Skill proposals: only when the user dictated or corrected a multi-step procedure they'll clearly want again. Most conversations have none.

Entity rules — copy these strings exactly:
{project_rule}
- A fact about a person: "person:<the first name they go by>"
- A fact about the user themselves or any other topic: omit entity

Reply with ONLY this JSON, no prose:
{{"observations":[{{"title":"short title","body":"the fact, one to three sentences","entity":"see entity rules (optional)","links":[],"supersedes":[]}}],"status":"what's in flight, or omit","skill_proposals":[{{"name":"kebab-case-name","description":"one line","instructions":"markdown instructions"}}]}}

All fields may be empty or absent. Titles must be short and distinctive — they double as link targets."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviews_validate_and_tolerate_missing_arrays() {
        let review: Review =
            serde_json::from_str(r#"{"observations":[{"title":"t","body":"b"}]}"#).unwrap();
        assert_eq!(review.observations.len(), 1);
        assert!(review.observations[0].entity.is_none());
        assert!(review.skill_proposals.is_empty());

        assert!(serde_json::from_str::<Review>(r#"{"observations":[{"title":"t"}]}"#).is_err());
    }

    #[test]
    fn skill_names_that_shape_paths_are_rejected() {
        assert!(valid_skill_name("deploy-check"));
        assert!(valid_skill_name("k8s"));
        assert!(!valid_skill_name("x/../../evil"));
        assert!(!valid_skill_name("-leading-dash"));
        assert!(!valid_skill_name("Uppercase"));
        assert!(!valid_skill_name(""));
        assert!(!valid_skill_name(&"a".repeat(65)));
    }

    #[test]
    fn superseding_archives_only_unpinned_observations() {
        use crate::memory::{Kind, Memory, NewMemory};

        let memory = Memory::open_in_memory().unwrap();
        let old = memory
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: None,
                title: "Old fact".into(),
                body: "Superseded soon.".into(),
                links: vec![],
                source_session: None,
            })
            .unwrap();
        let card = memory
            .add(&NewMemory {
                kind: Kind::Card,
                entity: Some("project:/x".into()),
                title: "A card".into(),
                body: "Body.".into(),
                links: vec![],
                source_session: None,
            })
            .unwrap();

        supersede(&memory, &[old]).unwrap();
        assert!(memory.get(old).unwrap().archived);
        assert!(supersede(&memory, &[card]).is_err());
        assert!(supersede(&memory, &[9999]).is_err());
    }

    #[test]
    fn titles_are_flattened_to_one_line() {
        assert_eq!(single_line("A  title\nwith\tbreaks"), "A title with breaks");
    }
}
