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

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cards;
use crate::embeddings;
use crate::flock;
use crate::memory::{Kind, Memory, NewObservation};
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
}

#[derive(Deserialize)]
struct Observation {
    title: String,
    body: String,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    links: Vec<String>,
}

#[derive(Deserialize)]
struct SkillProposal {
    name: String,
    description: String,
    instructions: String,
}

/// Called from hook handlers: decides cheaply whether a review is due and
/// spawns it detached — the hook must return within its timeout.
pub fn maybe_spawn(transcript_path: &str, config_dir: &Path, force: bool) -> Result<()> {
    let memory = Memory::open(&paths::memory_dir())?;
    let offset = memory.cursor(transcript_path)?;
    let (turns, _) = transcript::delta_since(Path::new(transcript_path), offset)?;

    let due = if force {
        !turns.is_empty()
    } else {
        transcript::user_turns(&turns) >= DEBOUNCE_USER_TURNS
    };
    if due {
        spawn_detached(transcript_path, config_dir)?;
    }
    Ok(())
}

fn spawn_detached(transcript_path: &str, config_dir: &Path) -> Result<()> {
    let agent = std::env::current_exe().context("could not determine the agent binary path")?;
    let mut command = Command::new(agent);
    command
        .args(["review", "--transcript", transcript_path, "--config-dir"])
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
    let mut child = command.spawn().context("could not spawn the reviewer")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub fn run(transcript_path: &Path, config_dir: &Path) -> Result<()> {
    let Some(_lock) = flock::try_acquire(&paths::memory_dir().join("reviewer.lock"))? else {
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

    let reviewed = review(&transcript::distill(&turns), config_dir)
        .and_then(|it| apply(&memory, &it).map(|()| it));
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

fn review(distilled: &str, config_dir: &Path) -> Result<Review> {
    let mut command = Command::new("claude");
    command
        .args(["-p", REVIEW_PROMPT, "--model", "haiku", "--output-format", "json"])
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
        .write_all(distilled.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("claude exited with {}", output.status);
    }

    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("claude's --output-format json envelope did not parse")?;
    let result = envelope["result"]
        .as_str()
        .context("claude's output carried no result field")?;
    serde_json::from_str(strip_fences(result)).context("the review was not the expected JSON")
}

fn apply(memory: &Memory, review: &Review) -> Result<()> {
    memory.connection.execute_batch("BEGIN IMMEDIATE")?;
    let applied = try_apply(memory, review);
    match applied {
        Ok(()) => memory.connection.execute_batch("COMMIT")?,
        Err(_) => memory.connection.execute_batch("ROLLBACK")?,
    }
    applied
}

fn try_apply(memory: &Memory, review: &Review) -> Result<()> {
    for observation in &review.observations {
        let mut links = cards::extract_links(&observation.body);
        for link in &observation.links {
            if !links.contains(link) {
                links.push(link.clone());
            }
        }
        let id = memory.add(&NewObservation {
            kind: Kind::Observation,
            entity: observation.entity.clone(),
            title: observation.title.clone(),
            body: observation.body.clone(),
            links,
            source_session: None,
        })?;
        embeddings::embed_into(memory, id, &format!("{}\n{}", observation.title, observation.body))?;
    }

    for proposal in &review.skill_proposals {
        memory.add_generated_skill(&proposal.name, &proposal.description, &proposal.instructions)?;
    }
    Ok(())
}

pub fn strip_fences(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let body = rest.split_once('\n').map(|it| it.1).unwrap_or(rest);
    body.strip_suffix("```").unwrap_or(body).trim()
}

fn log(message: &str) {
    let path = paths::logs_dir().join("reviewer.log");
    let _ = std::fs::create_dir_all(paths::logs_dir());
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {message}", crate::timestamp());
    }
}

const REVIEW_PROMPT: &str = r#"You are reviewing a conversation between a user and their coding assistant. The conversation transcript is on stdin.

Extract two things:
1. Observations: durable facts worth remembering across sessions — the user's preferences, corrections they gave, facts about themselves or their projects, decisions made. Skip anything session-specific or derivable from the code itself.
2. Skill proposals: repeatable multi-step workflows the user asked for that would be worth automating as a reusable skill. Most conversations have none.

Reply with ONLY this JSON, no prose:
{"observations":[{"title":"short title","body":"the fact, one to three sentences","entity":"project:/path or person:name (optional)","links":[]}],"skill_proposals":[{"name":"kebab-case-name","description":"one line","instructions":"markdown instructions"}]}

Both arrays may be empty. Titles must be short and distinctive — they double as link targets."#;

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
    fn fenced_json_is_unwrapped() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }
}
