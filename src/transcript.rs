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
use std::path::{Path, PathBuf};

use crate::hook_protocol::Tool;

pub struct Turn {
    pub role: Role,
    pub text: String,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Role {
    User,
    Assistant,
}

pub struct Delta {
    pub context: Vec<Turn>,
    pub new: Vec<Turn>,
    /// How far the cursor should advance. File sources measure it in bytes;
    /// opencode measures it as the last message id it consumed.
    pub cursor: Cursor,
}

#[derive(Clone)]
pub enum Cursor {
    Bytes(u64),
    Token(String),
}

/// Where a session's turns live. Claude, codex, and pi write line-JSON files;
/// opencode keeps its conversation in SQLite and is addressed by session id.
/// The cursor key is what the store's `cursors` table is keyed by.
#[derive(Clone)]
pub enum Source {
    File { tool: Tool, path: PathBuf },
    Opencode { session_id: String },
}

impl Source {
    pub fn cursor_key(&self) -> String {
        match self {
            Source::File { path, .. } => path.display().to_string(),
            Source::Opencode { session_id } => format!("opencode:{session_id}"),
        }
    }
}

/// Claude's own line parser over the generic reader.
pub fn read_delta(path: &Path, offset: u64, context_limit: usize) -> Result<Delta> {
    read_delta_with(path, offset, context_limit, claude_turn_from)
}

/// Everything past the cursor as new turns, plus up to `context_limit` turns
/// from just before it — the reviewer shows those as context-only so a "yes,
/// make that my default" at a chunk boundary still resolves. `parse` is the
/// per-tool line reader; everything else — the byte cursor, the shrink reset,
/// the unfinished-tail stop — is format-independent.
pub fn read_delta_with(
    path: &Path,
    offset: u64,
    context_limit: usize,
    parse: impl Fn(&Value) -> Option<Turn>,
) -> Result<Delta> {
    // A session can end before its tool writes the transcript — no file, no delta
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(empty_delta(offset));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not read the transcript {}", path.display()));
        }
    };
    // A shorter file than the cursor means the transcript was rewritten —
    // start over rather than staying silent forever
    let offset = if offset as usize > contents.len() { 0 } else { offset };

    let mut context = Vec::new();
    let mut new = Vec::new();
    let mut position = 0usize;
    let mut consumed = offset as usize;

    for line in contents.split_inclusive(|it| *it == b'\n') {
        let complete = line.ends_with(b"\n");
        let before_cursor = position + line.len() <= offset as usize;
        position += line.len();

        if !complete {
            break;
        }
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            if !before_cursor {
                consumed = position;
            }
            continue;
        };
        if before_cursor {
            context.extend(parse(&record));
        } else {
            new.extend(parse(&record));
            consumed = position;
        }
    }

    if context.len() > context_limit {
        context.drain(..context.len() - context_limit);
    }
    Ok(Delta {
        context,
        new,
        cursor: Cursor::Bytes(consumed as u64),
    })
}

pub fn empty_delta(offset: u64) -> Delta {
    Delta {
        context: Vec::new(),
        new: Vec::new(),
        cursor: Cursor::Bytes(offset),
    }
}

fn claude_turn_from(record: &Value) -> Option<Turn> {
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

/// Content that is either a bare string or an array of typed blocks; the
/// array form keeps only `text` blocks. Shared by the claude and pi parsers,
/// which both use this shape.
pub fn text_blocks(content: &Value) -> String {
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

pub fn new_turn(role: Role, text: String) -> Option<Turn> {
    if text.trim().is_empty() {
        None
    } else {
        Some(Turn { role, text })
    }
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

    fn bytes(cursor: &Cursor) -> u64 {
        match cursor {
            Cursor::Bytes(offset) => *offset,
            Cursor::Token(_) => panic!("expected a byte cursor"),
        }
    }

    #[test]
    fn deltas_skip_tool_blocks_and_incomplete_lines() {
        let path = fixture("deltas");
        let delta = read_delta(&path, 0, 0).unwrap();

        assert_eq!(delta.new.len(), 2);
        assert_eq!(user_turns(&delta.new), 1);
        assert_eq!(delta.new[0].text, "I prefer rebase over merge");
        assert_eq!(delta.new[1].text, "Rebased the branch.");

        let offset = bytes(&delta.cursor);
        let again = read_delta(&path, offset, 0).unwrap();
        assert!(again.new.is_empty());
        assert_eq!(bytes(&again.cursor), offset);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn context_turns_come_from_before_the_cursor() {
        let path = fixture("context");
        let offset = bytes(&read_delta(&path, 0, 0).unwrap().cursor);
        // The transcript grows past the incomplete tail line it had at review time
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"I prefer rebase over merge"}}"#, "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#, "\n",
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Rebased the branch."}]}}"#, "\n",
                r#"{"type":"user","message":{"role":"user","content":"yes, make that my default"}}"#, "\n",
            ),
        )
        .unwrap();

        let delta = read_delta(&path, offset, 2).unwrap();
        assert_eq!(delta.context.len(), 2);
        assert_eq!(delta.context[1].text, "Rebased the branch.");
        assert_eq!(delta.new.len(), 1);
        assert_eq!(delta.new[0].text, "yes, make that my default");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_transcripts_and_shrunken_files_recover() {
        let missing = std::env::temp_dir().join("agent-no-such-transcript.jsonl");
        let delta = read_delta(&missing, 42, 0).unwrap();
        assert!(delta.new.is_empty());
        assert_eq!(bytes(&delta.cursor), 42);

        let path = fixture("recovery");
        let delta = read_delta(&path, 1_000_000, 0).unwrap();
        assert_eq!(delta.new.len(), 2);
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
