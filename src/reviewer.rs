//! The reviewer: a cheap background pass that turns conversations into
//! memories.
//!
//! Capture happens in two phases. When a session stops, the transcript delta
//! is distilled into a durable chunk — numbered turns, with a couple of
//! context-only turns from before the cursor — and the cursor advances the
//! moment the chunk is safely queued, so nothing is ever reviewed twice or
//! lost to a crash. A drainer then leases chunks one at a time, shows haiku
//! what the store already knows (the project's card and status plus memories
//! retrieved against the conversation itself), and applies the validated
//! reply in one transaction. Every observation must cite the numbered turns
//! that justify it; supersedes and retracts may only name ids the model was
//! actually shown. Failed chunks retry with backoff and end up `dead` for
//! inspection, never silently discarded.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::cards;
use crate::distiller;
use crate::embeddings;
use crate::flock;
use crate::hook_protocol::Tool;
use crate::logs;
use crate::memory::{CLASSES, Kind, Memory, NewMemory, NewReviewChunk, ReviewChunk};
use crate::paths;
use crate::search;
use crate::transcript::{self, Source};
use crate::transcript_codex;
use crate::transcript_opencode;
use crate::transcript_pi;

const DEBOUNCE_USER_TURNS: usize = 5;
const CONTEXT_TURNS: usize = 2;
const ATTEMPT_CAP: i64 = 3;
const LEASE_MINUTES: u64 = 10;
const KNOWN_MEMORIES: usize = 12;
const KNOWN_BUDGET_CHARS: usize = 4000;
const EVIDENCE_EXCERPT_CHARS: usize = 250;

#[derive(Deserialize)]
struct Review {
    #[serde(default)]
    observations: Vec<Observation>,
    #[serde(default)]
    skill_proposals: Vec<SkillProposal>,
    #[serde(default)]
    status: Option<StatusChange>,
    #[serde(default)]
    retracts: Vec<Retraction>,
}

#[derive(Deserialize)]
struct Observation {
    title: String,
    body: String,
    class: String,
    evidence_turns: Vec<String>,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    links: Vec<String>,
    #[serde(default)]
    supersedes: Vec<i64>,
}

#[derive(Deserialize)]
struct StatusChange {
    op: String,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Deserialize)]
struct Retraction {
    id: i64,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct SkillProposal {
    name: String,
    description: String,
    instructions: String,
}

#[derive(Serialize, Deserialize)]
struct LabeledTurn {
    label: String,
    role: String,
    text: String,
}

/// Called on Stop: spawns a detached review once enough of the conversation
/// has accumulated — the hook must return within its timeout, so the review
/// itself never runs inline.
pub fn maybe_spawn(source: &Source, config_dir: &Path, cwd: Option<&str>) -> Result<()> {
    let turns = unreviewed_turns(source)?;
    if transcript::user_turns(&turns) >= DEBOUNCE_USER_TURNS {
        spawn_detached(source, config_dir, cwd)?;
    }
    Ok(())
}

/// Called on SessionEnd, and for every session the supervisor saw once the
/// child exits — the conversation will fire no more events, so anything
/// unreviewed gets queued now, debounce or no debounce.
pub fn spawn_final_review(source: &Source, config_dir: &Path, cwd: Option<&str>) -> Result<()> {
    if !unreviewed_turns(source)?.is_empty() {
        spawn_detached(source, config_dir, cwd)?;
    }
    Ok(())
}

fn unreviewed_turns(source: &Source) -> Result<Vec<transcript::Turn>> {
    let memory = Memory::open(&paths::memory_dir())?;
    Ok(read_delta(&memory, source, 0)?.new)
}

/// The delta for a source, whichever kind it is — a byte cursor over a JSONL
/// file for claude, codex, and pi; a message-id token over SQLite for
/// opencode.
fn read_delta(memory: &Memory, source: &Source, context_limit: usize) -> Result<transcript::Delta> {
    match source {
        Source::File { tool: Tool::Codex, path } => {
            let offset = memory.cursor(&source.cursor_key())?;
            transcript::read_delta_with(path, offset, context_limit, transcript_codex::turn_from)
        }
        Source::File { tool: Tool::Pi, path } => {
            let offset = memory.cursor(&source.cursor_key())?;
            transcript::read_delta_with(path, offset, context_limit, transcript_pi::turn_from)
        }
        Source::File { path, .. } => {
            let offset = memory.cursor(&source.cursor_key())?;
            transcript::read_delta(path, offset, context_limit)
        }
        Source::Opencode { session_id } => {
            let token = memory.cursor_token(&source.cursor_key())?;
            transcript_opencode::delta_since(
                &paths::opencode_db(),
                session_id,
                token.as_deref(),
                context_limit,
            )
        }
    }
}

fn spawn_detached(source: &Source, config_dir: &Path, cwd: Option<&str>) -> Result<()> {
    let agent = std::env::current_exe().context("could not determine the katami binary path")?;
    let mut command = Command::new(agent);
    command
        .args(["review", "--config-dir"])
        .arg(config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match source {
        Source::File { tool, path } => {
            command.args(["--tool", tool.as_str(), "--transcript"]).arg(path);
        }
        Source::Opencode { session_id } => {
            command.args(["--tool", "opencode", "--session", session_id]);
        }
    }
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

pub fn run(source: &Source, config_dir: &Path, cwd: Option<&Path>) -> Result<()> {
    enqueue(source, cwd)?;
    drain(config_dir)
}

/// Turns the delta into a durable chunk and advances the cursor in the same
/// transaction — from here on, a crash costs a retry, not the data.
fn enqueue(source: &Source, cwd: Option<&Path>) -> Result<()> {
    let memory = Memory::open(&paths::memory_dir())?;
    let delta = read_delta(&memory, source, CONTEXT_TURNS)?;
    if delta.new.is_empty() {
        return Ok(());
    }

    let entity = cwd.map(|it| {
        let canonical = cards::canonical_project_entity(it);
        let _ = memory.record_alias(&cards::project_entity(it), &canonical);
        canonical
    });
    let key = source.cursor_key();
    let chunk = NewReviewChunk {
        transcript_path: key.clone(),
        source_session: source_session(source),
        project_entity: entity,
        turns: serde_json::to_string(&label_turns(&delta))?,
    };
    memory.with_transaction(|memory| {
        memory.enqueue_review_chunk(&chunk)?;
        match &delta.cursor {
            transcript::Cursor::Bytes(offset) => memory.set_cursor(&key, *offset),
            transcript::Cursor::Token(token) => memory.set_cursor_token(&key, token),
        }
    })
}

/// A stable label for the session a chunk came from — the transcript file's
/// stem, or the opencode session id.
fn source_session(source: &Source) -> Option<String> {
    match source {
        Source::File { path, .. } => path.file_stem().map(|it| it.to_string_lossy().to_string()),
        Source::Opencode { session_id } => Some(session_id.clone()),
    }
}

fn label_turns(delta: &transcript::Delta) -> Vec<LabeledTurn> {
    let mut labeled = Vec::new();
    for (index, turn) in delta.context.iter().enumerate() {
        labeled.push(LabeledTurn {
            label: format!("C{}", index + 1),
            role: role_of(turn),
            text: turn.text.clone(),
        });
    }
    for (index, turn) in delta.new.iter().enumerate() {
        labeled.push(LabeledTurn {
            label: format!("N{}", index + 1),
            role: role_of(turn),
            text: turn.text.clone(),
        });
    }
    labeled
}

fn role_of(turn: &transcript::Turn) -> String {
    match turn.role {
        transcript::Role::User => "user".to_string(),
        transcript::Role::Assistant => "assistant".to_string(),
    }
}

/// Processes queued chunks until none are due. One drainer at a time; a
/// second caller finding the lock held can leave — the holder will reach its
/// chunk, and anything it misses waits for the next stop or the curator.
pub fn drain(config_dir: &Path) -> Result<()> {
    let Some(_lock) = flock::try_acquire(&paths::memory_dir().join("reviewer.lock"))? else {
        return Ok(());
    };
    let memory = Memory::open(&paths::memory_dir())?;

    while let Some(chunk) = memory.lease_review_chunk(LEASE_MINUTES)? {
        match review_chunk(&memory, &chunk, config_dir) {
            Ok(()) => memory.complete_review_chunk(chunk.id)?,
            Err(error) => {
                memory.fail_review_chunk(chunk.id, chunk.attempts, &format!("{error:#}"), ATTEMPT_CAP)?;
                log(&format!("chunk {} failed (attempt {}): {error:#}", chunk.id, chunk.attempts + 1));
            }
        }
    }
    Ok(())
}

fn review_chunk(memory: &Memory, chunk: &ReviewChunk, config_dir: &Path) -> Result<()> {
    let turns: Vec<LabeledTurn> =
        serde_json::from_str(&chunk.turns).context("the stored chunk turns did not parse")?;
    let entity = chunk.project_entity.as_deref();

    let (known, shown_ids) = known_context(memory, entity, &turns)?;
    let input = format!("{known}\nTranscript:\n{}", render_turns(&turns));
    let review: Review = distiller::ask(&review_prompt(entity), &input, config_dir)?;

    memory.with_transaction(|memory| apply(memory, &review, chunk, &turns, &shown_ids))?;
    report(&review);
    Ok(())
}

/// What the store already knows: the project's card and status, plus
/// memories retrieved against the conversation's own user turns — so global
/// preferences and people surface for superseding too, not just the current
/// project's observations.
fn known_context(
    memory: &Memory,
    entity: Option<&str>,
    turns: &[LabeledTurn],
) -> Result<(String, Vec<i64>)> {
    let mut known = String::from(
        "What you already know (do not restate; supersede or retract by id when the conversation changes an item):\n",
    );
    let mut shown_ids = Vec::new();

    if let Some(entity) = entity {
        if let Some(card) = memory.card_for_entity(entity)? {
            known.push_str(&format!(
                "Card for {entity} (not supersedable):\n{}\n",
                card.body.trim()
            ));
        }
        if let Some(status) = memory.status_for_entity(entity)? {
            known.push_str(&format!(
                "Current status (as of {}):\n{}\n",
                &status.updated[..10],
                status.body.trim()
            ));
        }
    }

    for hit in retrieve_for_turns(memory, turns)? {
        let stored = memory.get(hit.id)?;
        let line = format!(
            "[id {}; {}; entity {}] {}: {}\n",
            stored.id,
            stored.kind,
            stored.entity.as_deref().unwrap_or("none"),
            stored.title,
            stored.body
        );
        if known.len() + line.len() > KNOWN_BUDGET_CHARS {
            break;
        }
        known.push_str(&line);
        shown_ids.push(stored.id);
    }

    let entities = memory.entities()?;
    if !entities.is_empty() {
        known.push_str(&format!("Known entities: {}\n", entities.join(", ")));
    }
    Ok((known, shown_ids))
}

/// Hybrid retrieval keyed on the conversation itself: the last two
/// substantive user turns, searched separately and fused.
fn retrieve_for_turns(memory: &Memory, turns: &[LabeledTurn]) -> Result<Vec<search::Hit>> {
    let queries: Vec<&LabeledTurn> = turns
        .iter()
        .rev()
        .filter(|it| it.role == "user" && it.text.split_whitespace().count() >= 3)
        .take(2)
        .collect();

    let mut rankings = Vec::new();
    for query in queries {
        rankings.push(search::hybrid(memory, &query.text, KNOWN_MEMORIES)?);
    }
    Ok(search::fuse(&rankings, KNOWN_MEMORIES))
}

fn render_turns(turns: &[LabeledTurn]) -> String {
    turns
        .iter()
        .map(|it| format!("[{} {}] {}", it.label, it.role, it.text.trim()))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn apply(
    memory: &Memory,
    review: &Review,
    chunk: &ReviewChunk,
    turns: &[LabeledTurn],
    shown_ids: &[i64],
) -> Result<()> {
    for observation in &review.observations {
        apply_observation(memory, observation, chunk, turns, shown_ids)?;
    }

    for retraction in &review.retracts {
        assert_changeable(memory, retraction.id, shown_ids)?;
        memory.archive(retraction.id, "retracted")?;
        log(&format!(
            "retracted memory {} — {}",
            retraction.id,
            retraction.reason.as_deref().unwrap_or("no reason given")
        ));
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

    if let (Some(entity), Some(status)) = (chunk.project_entity.as_deref(), &review.status) {
        apply_status(memory, entity, status)?;
    }
    Ok(())
}

fn apply_observation(
    memory: &Memory,
    observation: &Observation,
    chunk: &ReviewChunk,
    turns: &[LabeledTurn],
    shown_ids: &[i64],
) -> Result<()> {
    if !CLASSES.contains(&observation.class.as_str()) {
        anyhow::bail!(
            "the review classified '{}' as '{}' — not one of the known classes",
            observation.title,
            observation.class
        );
    }
    let evidence = cited_turns(observation, turns)?;

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
        source_session: chunk.source_session.clone(),
        class: Some(observation.class.clone()),
    })?;
    embeddings::embed_into(memory, id, &format!("{title}\n{}", observation.body))?;

    for turn in evidence {
        let excerpt: String = turn.text.chars().take(EVIDENCE_EXCERPT_CHARS).collect();
        memory.add_evidence(id, chunk.source_session.as_deref(), &turn.label, &turn.role, &excerpt)?;
    }
    for superseded in &observation.supersedes {
        assert_changeable(memory, *superseded, shown_ids)?;
        memory.archive(*superseded, "superseded")?;
    }
    Ok(())
}

/// Evidence keeps capture honest: every observation must cite new turns that
/// exist, and anything personal must trace back to the user's own words.
fn cited_turns<'turns>(
    observation: &Observation,
    turns: &'turns [LabeledTurn],
) -> Result<Vec<&'turns LabeledTurn>> {
    if observation.evidence_turns.is_empty() {
        anyhow::bail!("observation '{}' cites no evidence turns", observation.title);
    }

    let mut cited = Vec::new();
    for label in &observation.evidence_turns {
        let turn = turns
            .iter()
            .find(|it| &it.label == label)
            .with_context(|| format!("observation '{}' cites turn {label}, which doesn't exist", observation.title))?;
        cited.push(turn);
    }

    if !cited.iter().any(|it| it.label.starts_with('N')) {
        anyhow::bail!(
            "observation '{}' cites only context turns — context can resolve references, not justify capture",
            observation.title
        );
    }
    let personal = matches!(observation.class.as_str(), "preference" | "identity" | "constraint");
    if personal && !cited.iter().any(|it| it.label.starts_with('N') && it.role == "user") {
        anyhow::bail!(
            "observation '{}' is a {} but cites no new user turn as evidence",
            observation.title,
            observation.class
        );
    }
    Ok(cited)
}

fn assert_changeable(memory: &Memory, id: i64, shown_ids: &[i64]) -> Result<()> {
    if !shown_ids.contains(&id) {
        anyhow::bail!("the review named id {id}, which it was never shown");
    }
    let stored = memory.get(id)?;
    if stored.kind != Kind::Observation || stored.pinned {
        anyhow::bail!("id {id} is not an unpinned observation — it can't be superseded or retracted");
    }
    Ok(())
}

fn apply_status(memory: &Memory, entity: &str, status: &StatusChange) -> Result<()> {
    match status.op.as_str() {
        "replace" => {
            let body = status
                .body
                .as_deref()
                .context("a status replace carried no body")?;
            if !body.trim().is_empty() {
                memory.upsert_status(entity, body)?;
            }
            Ok(())
        }
        "clear" => {
            if let Some(current) = memory.status_for_entity(entity)? {
                memory.archive(current.id, "completed_status")?;
            }
            Ok(())
        }
        other => anyhow::bail!("unknown status op '{other}' — expected replace or clear"),
    }
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

fn report(review: &Review) {
    for observation in &review.observations {
        log(&format!("observed [[{}]] ({})", observation.title, observation.class));
    }
    for proposal in &review.skill_proposals {
        log(&format!("proposed skill {}", proposal.name));
    }
}

fn log(message: &str) {
    logs::append("reviewer", message);
}

fn review_prompt(entity: Option<&str>) -> String {
    let project_rule = match entity {
        Some(entity) => format!(
            "- A fact about the project you're working in: \"{entity}\"\n  (this session's project — copy the string exactly, never invent a shorter name for it)"
        ),
        None => "- A fact about a specific project: omit entity — this session's project is unknown".to_string(),
    };

    format!(
        r#"You are reviewing a conversation between a user and their coding assistant. Stdin has two parts: what the memory store already knows, then the transcript as labeled turns — [C*] turns are context from before this chunk and may only resolve references; [N*] turns are new and are the only valid evidence.

Extract:
1. Observations: durable facts that will still be true in a month — the user's preferences, corrections they gave, facts about themselves, their projects, and the people they work with. NOT the state of in-progress work, and NOT anything the store already knows. Each observation carries:
   - "class": one of preference, constraint, identity, relationship, decision, history, reference
   - "evidence_turns": the turn labels that justify it, at least one N turn; preferences, constraints, and identity facts need a new USER turn
   - "supersedes": ids of shown [id N] items this observation replaces
   An observation's entity is what the fact is ABOUT, not where it was discussed — a fact about a tool, a service, or general practice gets no entity even when it came up inside a project. Skip anything derivable from the code itself, and skip discussion about this memory system.
2. Retracts: shown [id N] items the conversation revealed to be no longer true, with nothing replacing them.
3. Status: {{"op":"replace","body":"..."}} rewrites the project's in-flight-work snapshot wholesale (open PRs, half-done migrations, blocked work — restate anything still in flight); {{"op":"clear"}} says the in-flight work is finished; omit the field entirely when this conversation carries no evidence either way.
4. Skill proposals: only when the user dictated or corrected a multi-step procedure they'll clearly want again. Most conversations have none.

Entity rules — copy these strings exactly:
{project_rule}
- A fact about a person: "person:<the first name they go by>"
- A fact about the user themselves or any other topic: omit entity

Reply with ONLY this JSON, no prose:
{{"observations":[{{"title":"short title","body":"the fact, one to three sentences","class":"preference","evidence_turns":["N2"],"entity":"see entity rules (optional)","links":[],"supersedes":[]}}],"retracts":[{{"id":7,"reason":"why"}}],"status":{{"op":"replace","body":"..."}},"skill_proposals":[{{"name":"kebab-case-name","description":"one line","instructions":"markdown instructions"}}]}}

All fields may be empty or absent. Titles must be short and distinctive — they double as link targets."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Kind, Memory, NewMemory};

    fn observation(class: &str, evidence: &[&str]) -> Observation {
        Observation {
            title: "T".into(),
            body: "B".into(),
            class: class.into(),
            evidence_turns: evidence.iter().map(|it| it.to_string()).collect(),
            entity: None,
            links: vec![],
            supersedes: vec![],
        }
    }

    fn turns() -> Vec<LabeledTurn> {
        vec![
            LabeledTurn { label: "C1".into(), role: "assistant".into(), text: "context".into() },
            LabeledTurn { label: "N1".into(), role: "user".into(), text: "I prefer rebase".into() },
            LabeledTurn { label: "N2".into(), role: "assistant".into(), text: "Noted".into() },
        ]
    }

    #[test]
    fn reviews_validate_and_tolerate_missing_arrays() {
        let review: Review = serde_json::from_str(
            r#"{"observations":[{"title":"t","body":"b","class":"preference","evidence_turns":["N1"]}]}"#,
        )
        .unwrap();
        assert_eq!(review.observations.len(), 1);
        assert!(review.observations[0].entity.is_none());
        assert!(review.retracts.is_empty());

        assert!(serde_json::from_str::<Review>(r#"{"observations":[{"title":"t","body":"b"}]}"#).is_err());
    }

    #[test]
    fn evidence_must_name_real_new_turns() {
        let turns = turns();
        assert!(cited_turns(&observation("preference", &["N1"]), &turns).is_ok());
        assert!(cited_turns(&observation("preference", &[]), &turns).is_err());
        assert!(cited_turns(&observation("preference", &["N9"]), &turns).is_err());
        assert!(cited_turns(&observation("preference", &["C1"]), &turns).is_err());
        // an assistant-only citation can't justify a preference
        assert!(cited_turns(&observation("preference", &["N2"]), &turns).is_err());
        // but it can justify project history
        assert!(cited_turns(&observation("history", &["N2"]), &turns).is_ok());
    }

    #[test]
    fn changes_are_limited_to_shown_unpinned_observations() {
        let memory = Memory::open_in_memory().unwrap();
        let shown = memory
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: None,
                title: "Old fact".into(),
                body: "Superseded soon.".into(),
                links: vec![],
                source_session: None,
                class: None,
            })
            .unwrap();
        let unshown = memory
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: None,
                title: "Hidden".into(),
                body: "Never in context.".into(),
                links: vec![],
                source_session: None,
                class: None,
            })
            .unwrap();

        assert!(assert_changeable(&memory, shown, &[shown]).is_ok());
        assert!(assert_changeable(&memory, unshown, &[shown]).is_err());
        memory.archive(shown, "superseded").unwrap();
        assert_eq!(memory.get(shown).unwrap().archived, true);
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
    fn titles_are_flattened_to_one_line() {
        assert_eq!(single_line("A  title\nwith\tbreaks"), "A title with breaks");
    }
}
