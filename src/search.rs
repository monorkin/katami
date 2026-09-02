//! Retrieval: turning a prompt into the handful of memories worth injecting.
//!
//! BM25 over FTS5 carries exact-term recall; vector search (phase 4) joins in
//! through `fuse`. The composed context spends most of its budget on full
//! bodies for the top hits, then lists one-line `[[Title]]` pointers for their
//! 1-hop neighbors — the model can Read a card if a pointer looks relevant,
//! so discovery stays cheap instead of dumping the graph into every turn.

use anyhow::Result;

use crate::memory::{Memory, Stored};

const CONTEXT_BUDGET_CHARS: usize = 4000;

pub struct Hit {
    pub id: i64,
    pub score: f64,
}

pub fn bm25(memory: &Memory, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let Some(sanitized) = sanitize(query) else {
        return Ok(Vec::new());
    };

    // Status rows are deliberately unsearchable: current-state snapshots only
    // belong in their own project's SessionStart, never riding a keyword into
    // an unrelated conversation
    let mut statement = memory.connection.prepare(
        "SELECT f.rowid, bm25(memories_fts) FROM memories_fts f
         JOIN memories m ON m.id = f.rowid
         WHERE memories_fts MATCH ?1 AND m.archived = 0 AND m.kind != 'status'
         ORDER BY bm25(memories_fts) LIMIT ?2",
    )?;
    let rows = statement.query_map(rusqlite::params![sanitized, limit as i64], |row| {
        Ok(Hit {
            id: row.get(0)?,
            // bm25() returns better-is-more-negative; flip it so higher wins
            score: -row.get::<_, f64>(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// BM25 and vector rankings fused with reciprocal rank fusion. Falls back to
/// BM25 alone when no embedding model is installed.
pub fn hybrid(memory: &Memory, query: &str, limit: usize) -> Result<Vec<Hit>> {
    let lexical = bm25(memory, query, limit * 2)?;
    let Some(query_vector) = crate::embeddings::embed(query) else {
        return Ok(lexical.into_iter().take(limit).collect());
    };

    let semantic = vector(memory, &query_vector, limit * 2)?;
    Ok(fuse(&[lexical, semantic], limit))
}

pub fn vector(memory: &Memory, query: &[f32], limit: usize) -> Result<Vec<Hit>> {
    let mut scored: Vec<Hit> = memory
        .embeddings(crate::embeddings::MODEL_NAME)?
        .into_iter()
        .map(|(id, vector)| Hit {
            id,
            score: cosine(query, &vector),
        })
        .collect();
    scored.sort_by(|a, b| b.score.total_cmp(&a.score));
    scored.truncate(limit);
    Ok(scored)
}

pub fn fuse(rankings: &[Vec<Hit>], limit: usize) -> Vec<Hit> {
    let mut scores: Vec<(i64, f64)> = Vec::new();
    for ranking in rankings {
        for (rank, hit) in ranking.iter().enumerate() {
            let contribution = 1.0 / (60.0 + rank as f64 + 1.0);
            match scores.iter_mut().find(|(id, _)| *id == hit.id) {
                Some((_, score)) => *score += contribution,
                None => scores.push((hit.id, contribution)),
            }
        }
    }
    scores.sort_by(|a, b| b.1.total_cmp(&a.1));
    scores
        .into_iter()
        .take(limit)
        .map(|(id, score)| Hit { id, score })
        .collect()
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        dot += (*x as f64) * (*y as f64);
        norm_a += (*x as f64).powi(2);
        norm_b += (*y as f64).powi(2);
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a.sqrt() * norm_b.sqrt())
    }
}

/// FTS5 query syntax treats bare punctuation as operators, so the prompt is
/// reduced to quoted words OR'd together.
fn sanitize(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split(|it: char| !it.is_alphanumeric())
        .filter(|it| it.len() >= 2)
        .take(24)
        .map(|it| format!("\"{it}\""))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

pub fn compose_context(memory: &Memory, hits: &[Hit]) -> Result<Option<String>> {
    if hits.is_empty() {
        return Ok(None);
    }

    let mut context = String::from("Relevant memories:\n");
    let mut pointers: Vec<String> = Vec::new();
    let body_budget = CONTEXT_BUDGET_CHARS * 3 / 4;

    for hit in hits {
        let stored = memory.get(hit.id)?;
        let entry = format!("\n## {}\n{}\n", stored.title, stored.body.trim());
        if context.len() + entry.len() <= body_budget {
            context.push_str(&entry);
            for neighbor in memory.neighbors(hit.id)? {
                let pointer = pointer_line(&neighbor);
                if !pointers.contains(&pointer) {
                    pointers.push(pointer);
                }
            }
        }
    }

    for pointer in pointers {
        if context.len() + pointer.len() > CONTEXT_BUDGET_CHARS {
            break;
        }
        context.push_str(&pointer);
    }
    Ok(Some(context))
}

fn pointer_line(neighbor: &Stored) -> String {
    let first_sentence = neighbor
        .body
        .split(['.', '\n'])
        .next()
        .unwrap_or("")
        .trim();
    format!("\nLinked: [[{}]] — {first_sentence}", neighbor.title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Kind, NewMemory};

    fn seeded() -> Memory {
        let memory = Memory::open_in_memory().unwrap();
        memory
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: Some("project:agent".into()),
                title: "Supervisor design".into(),
                body: "The supervisor never parses the pty byte stream.".into(),
                links: vec!["Hook protocol".into()],
                source_session: None,
            })
            .unwrap();
        memory
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: None,
                title: "Hook protocol".into(),
                body: "Newline-delimited JSON. One request per connection.".into(),
                links: vec![],
                source_session: None,
            })
            .unwrap();
        memory
    }

    #[test]
    fn bm25_finds_by_content_words() {
        let memory = seeded();
        let hits = bm25(&memory, "how does the pty supervisor work?", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(memory.get(hits[0].id).unwrap().title, "Supervisor design");
    }

    #[test]
    fn punctuation_only_queries_match_nothing() {
        let memory = seeded();
        assert!(bm25(&memory, "?! -- ((", 5).unwrap().is_empty());
        assert!(bm25(&memory, "", 5).unwrap().is_empty());
    }

    #[test]
    fn context_includes_bodies_and_neighbor_pointers() {
        let memory = seeded();
        let hits = bm25(&memory, "pty stream supervisor", 5).unwrap();
        let context = compose_context(&memory, &hits).unwrap().unwrap();

        assert!(context.contains("## Supervisor design"));
        assert!(context.contains("never parses the pty"));
        assert!(context.contains("Linked: [[Hook protocol]] — Newline-delimited JSON"));
        assert!(context.len() <= CONTEXT_BUDGET_CHARS + 100);
    }

    #[test]
    fn empty_hits_compose_no_context() {
        let memory = seeded();
        assert!(compose_context(&memory, &[]).unwrap().is_none());
    }

    #[test]
    fn fusion_prefers_agreement_over_a_single_top_rank() {
        let lexical = vec![Hit { id: 1, score: 9.0 }, Hit { id: 2, score: 5.0 }];
        let semantic = vec![Hit { id: 2, score: 0.9 }, Hit { id: 3, score: 0.8 }];

        let fused = fuse(&[lexical, semantic], 3);
        assert_eq!(fused[0].id, 2);
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn cosine_handles_degenerate_vectors() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), 1.0);
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
    }
}
