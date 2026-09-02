//! Reading opencode conversations out of its SQLite store.
//!
//! opencode keeps no transcript file — messages and their parts live in
//! `~/.local/share/opencode/opencode.db`. We open it read-only (a second
//! connection, never the memory store's) so we never block opencode's writer,
//! and cursor on the message id, which is lexicographically monotonic. An
//! assistant message counts only once `time.completed` is set — an in-flight
//! one ends the scan so nothing streams in half-read. Synthetic parts (our
//! own injected memory) are skipped: the structural self-ingestion guard.
//! Compaction and revert cascade-delete rows; if the cursor's row is gone,
//! we restart from the beginning rather than stall.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::path::Path;

use crate::transcript::{Cursor, Delta, Role, Turn};

pub fn delta_since(
    db: &Path,
    session_id: &str,
    token: Option<&str>,
    context_limit: usize,
) -> Result<Delta> {
    if !db.exists() {
        return Ok(empty(token));
    }
    let connection = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("could not open {}", db.display()))?;

    // A cursor whose row was cascaded away would silently skip everything
    // after it — restart from the beginning instead.
    let token = match token {
        Some(id) if !message_exists(&connection, session_id, id)? => None,
        other => other,
    };

    let mut messages = completed_messages(&connection, session_id)?;
    let split = token
        .and_then(|id| messages.iter().position(|it| it.id == id))
        .map(|position| position + 1)
        .unwrap_or(0);

    let new: Vec<Turn> = messages.split_off(split).into_iter().map(|it| it.turn).collect();
    let end_token = new
        .is_empty()
        .then(|| token.map(str::to_string))
        .flatten()
        .or_else(|| last_completed_id(&connection, session_id).ok().flatten());

    let context_start = messages.len().saturating_sub(context_limit);
    let context = messages.split_off(context_start).into_iter().map(|it| it.turn).collect();

    Ok(Delta {
        context,
        new,
        cursor: match end_token {
            Some(token) => Cursor::Token(token),
            None => Cursor::Token(token.unwrap_or("").to_string()),
        },
    })
}

struct Message {
    id: String,
    turn: Turn,
}

/// Every completed message in the session, oldest first, with its text parts
/// joined and synthetic (injected) parts skipped.
fn completed_messages(connection: &Connection, session_id: &str) -> Result<Vec<Message>> {
    let mut statement = connection.prepare(
        "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY id",
    )?;
    let rows = statement.query_map([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut messages = Vec::new();
    for row in rows {
        let (id, data) = row?;
        let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let role = match data["role"].as_str() {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        // An in-flight assistant message ends the scan — parts still stream in
        if role == Role::Assistant && data["time"]["completed"].is_null() {
            break;
        }
        let text = message_text(connection, &id)?;
        if !text.trim().is_empty() {
            messages.push(Message { id, turn: Turn { role, text } });
        }
    }
    Ok(messages)
}

fn message_text(connection: &Connection, message_id: &str) -> Result<String> {
    let mut statement = connection
        .prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY id")?;
    let rows = statement.query_map([message_id], |row| row.get::<_, String>(0))?;

    let mut pieces = Vec::new();
    for row in rows {
        let data: Value = serde_json::from_str(&row?).unwrap_or(Value::Null);
        if data["type"] == "text" && data["synthetic"] != Value::Bool(true) {
            if let Some(text) = data["text"].as_str() {
                pieces.push(text.to_string());
            }
        }
    }
    Ok(pieces.join("\n"))
}

fn message_exists(connection: &Connection, session_id: &str, id: &str) -> Result<bool> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM message WHERE session_id = ?1 AND id = ?2",
        [session_id, id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn last_completed_id(connection: &Connection, session_id: &str) -> Result<Option<String>> {
    Ok(completed_messages(connection, session_id)?
        .last()
        .map(|it| it.id.clone()))
}

fn empty(token: Option<&str>) -> Delta {
    Delta {
        context: Vec::new(),
        new: Vec::new(),
        cursor: Cursor::Token(token.unwrap_or("").to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("agent-opencode-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT);",
            )
            .unwrap();
        path
    }

    fn add_message(db: &Path, id: &str, session: &str, data: &str) {
        let connection = Connection::open(db).unwrap();
        connection
            .execute(
                "INSERT INTO message (id, session_id, data) VALUES (?1, ?2, ?3)",
                [id, session, data],
            )
            .unwrap();
    }

    fn add_part(db: &Path, id: &str, message: &str, session: &str, data: &str) {
        let connection = Connection::open(db).unwrap();
        connection
            .execute(
                "INSERT INTO part (id, message_id, session_id, data) VALUES (?1, ?2, ?3, ?4)",
                [id, message, session, data],
            )
            .unwrap();
    }

    #[test]
    fn reads_completed_turns_skips_synthetic_and_incomplete() {
        let path = fixture("reads");
        let session = "ses_1";

        add_message(&path, "msg_001", session, r#"{"role":"user","time":{"created":1}}"#);
        add_part(&path, "prt_001a", "msg_001", session, r#"{"type":"text","text":"real prompt"}"#);
        add_part(&path, "prt_001b", "msg_001", session, r#"{"type":"text","text":"injected","synthetic":true}"#);
        add_message(&path, "msg_002", session, r#"{"role":"assistant","time":{"created":2,"completed":3}}"#);
        add_part(&path, "prt_002a", "msg_002", session, r#"{"type":"text","text":"done"}"#);
        // in-flight assistant message: no completed time, must halt the scan
        add_message(&path, "msg_003", session, r#"{"role":"assistant","time":{"created":4}}"#);
        add_part(&path, "prt_003a", "msg_003", session, r#"{"type":"text","text":"streaming"}"#);

        let delta = delta_since(&path, session, None, 0).unwrap();
        assert_eq!(delta.new.len(), 2);
        assert_eq!(delta.new[0].text, "real prompt");
        assert_eq!(delta.new[1].text, "done");
        match &delta.cursor {
            Cursor::Token(token) => assert_eq!(token, "msg_002"),
            _ => panic!("expected a token cursor"),
        }

        // second pass from that token sees nothing new
        let again = delta_since(&path, session, Some("msg_002"), 0).unwrap();
        assert!(again.new.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_vanished_cursor_restarts_from_the_beginning() {
        let path = fixture("vanished");
        let session = "ses_1";
        add_message(&path, "msg_010", session, r#"{"role":"user","time":{"created":1}}"#);
        add_part(&path, "prt_010a", "msg_010", session, r#"{"type":"text","text":"hi"}"#);

        let delta = delta_since(&path, session, Some("msg_999_gone"), 0).unwrap();
        assert_eq!(delta.new.len(), 1);
        let _ = std::fs::remove_file(path);
    }
}
