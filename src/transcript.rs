//! Reading Claude Code transcripts incrementally.
//!
//! The reviewer never re-reads a conversation: a byte cursor per transcript
//! marks how far it got, and only the delta past it is parsed. The cursor
//! only ever advances past complete lines, so a transcript caught mid-write
//! just leaves its unfinished tail for the next pass. Distillation keeps what
//! the reviewer needs — what the user said and what claude concluded — and
//! drops the tool traffic, which is most of the bytes and none of the signal.

use anyhow::{Context, Result};
use serde_json::Value;
use std::path::Path;

pub struct Turn {
    pub role: Role,
    pub text: String,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Role {
    User,
    Assistant,
}

pub fn delta_since(path: &Path, offset: u64) -> Result<(Vec<Turn>, u64)> {
    // A session can end before Claude Code writes its transcript — no file, no delta
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), offset));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not read the transcript {}", path.display()));
        }
    };
    // A shorter file than the cursor means the transcript was rewritten —
    // start over rather than staying silent forever
    let offset = if offset as usize > contents.len() { 0 } else { offset };
    if offset as usize >= contents.len() {
        return Ok((Vec::new(), offset));
    }

    let delta = &contents[offset as usize..];
    let mut turns = Vec::new();
    let mut consumed = 0usize;

    for line in delta.split_inclusive(|it| *it == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        }
        consumed += line.len();
        if let Ok(record) = serde_json::from_slice::<Value>(line) {
            turns.extend(turn_from(&record));
        }
    }
    Ok((turns, offset + consumed as u64))
}

fn turn_from(record: &Value) -> Option<Turn> {
    let role = match record["type"].as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };

    let text = strip_injected(&text_blocks(&record["message"]["content"]));
    if text.is_empty() {
        None
    } else {
        Some(Turn { role, text })
    }
}

/// Context the supervisor injected comes back through the transcript inside
/// user turns. Left in place, every injected memory would echo through the
/// reviewer and the store would slowly ingest its own output — so the
/// sentinel-wrapped spans are cut before distillation.
fn strip_injected(text: &str) -> String {
    const OPEN: &str = "<agent-memory>";
    const CLOSE: &str = "</agent-memory>";

    let mut remaining = text;
    let mut stripped = String::new();
    while let Some(start) = remaining.find(OPEN) {
        stripped.push_str(&remaining[..start]);
        match remaining[start..].find(CLOSE) {
            Some(end) => remaining = &remaining[start + end + CLOSE.len()..],
            None => {
                remaining = "";
            }
        }
    }
    stripped.push_str(remaining);
    stripped.trim().to_string()
}

fn text_blocks(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|it| it["type"] == "text")
            .filter_map(|it| it["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub fn distill(turns: &[Turn]) -> String {
    turns
        .iter()
        .map(|turn| {
            let speaker = match turn.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            format!("{speaker}: {}", turn.text.trim())
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn user_turns(turns: &[Turn]) -> usize {
    turns.iter().filter(|it| it.role == Role::User).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("agent-transcript-{name}-{}.jsonl", std::process::id()));
        let contents = concat!(
            r#"{"type":"user","message":{"role":"user","content":"I prefer rebase over merge"}}"#, "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#, "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Rebased the branch."}]}}"#, "\n",
            r#"{"type":"user","message":{"incomplete"#,
        );
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn deltas_skip_tool_blocks_and_incomplete_lines() {
        let path = fixture("deltas");
        let (turns, offset) = delta_since(&path, 0).unwrap();

        assert_eq!(turns.len(), 2);
        assert_eq!(user_turns(&turns), 1);
        assert!(distill(&turns).starts_with("User: I prefer rebase"));
        assert!(distill(&turns).contains("Assistant: Rebased the branch."));

        let (again, unchanged) = delta_since(&path, offset).unwrap();
        assert!(again.is_empty());
        assert_eq!(unchanged, offset);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_transcripts_and_shrunken_files_recover() {
        let missing = std::env::temp_dir().join("agent-no-such-transcript.jsonl");
        let (turns, offset) = delta_since(&missing, 42).unwrap();
        assert!(turns.is_empty());
        assert_eq!(offset, 42);

        let path = fixture("recovery");
        let (turns, _) = delta_since(&path, 1_000_000).unwrap();
        assert_eq!(turns.len(), 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn injected_memory_spans_are_stripped() {
        assert_eq!(
            strip_injected("before <agent-memory>\ninjected stuff\n</agent-memory> after"),
            "before  after"
        );
        assert_eq!(strip_injected("<agent-memory>all of it"), "");
        assert_eq!(strip_injected("untouched"), "untouched");
    }
}
