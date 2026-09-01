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

    let text = text_blocks(&record["message"]["content"]);
    if text.is_empty() {
        None
    } else {
        Some(Turn { role, text })
    }
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

    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!("agent-transcript-{}.jsonl", std::process::id()));
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
        let path = fixture();
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
}
