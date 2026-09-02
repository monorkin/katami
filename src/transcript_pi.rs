//! Parsing pi session transcripts.
//!
//! pi writes `~/.pi/agent/sessions/--<cwd-dashed>--/<ts>_<uuid>.jsonl`, strict
//! JSONL: a `session` header line, then `message` entries and others. Our
//! injected memory persists as a `custom_message` entry, so skipping every
//! type but `message` is the structural self-ingestion guard. User content is
//! a string or an array of blocks; assistant content is always an array —
//! `text_blocks` handles both. Branching (`parentId`) is ignored: line order
//! reads as an append log, which is all the reviewer needs.

use serde_json::Value;

use crate::transcript::{self, Role, Turn};

pub fn turn_from(record: &Value) -> Option<Turn> {
    if record["type"].as_str()? != "message" {
        return None;
    }
    let message = &record["message"];
    let role = match message["role"].as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    transcript::new_turn(role, transcript::text_blocks(&message["content"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn handles_string_and_array_content() {
        let user = record(
            r#"{"type":"message","message":{"role":"user","content":"I prefer rebase"}}"#,
        );
        let assistant = record(
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Rebased."},{"type":"toolCall","name":"bash"}]}}"#,
        );
        assert_eq!(turn_from(&user).unwrap().text, "I prefer rebase");
        assert_eq!(turn_from(&assistant).unwrap().text, "Rebased.");
    }

    #[test]
    fn skips_header_and_injected_custom_messages() {
        let header = record(r#"{"type":"session","version":3,"cwd":"/x"}"#);
        let injected = record(
            r#"{"type":"custom_message","message":{"customType":"agent-memory","content":"injected"}}"#,
        );
        assert!(turn_from(&header).is_none());
        assert!(turn_from(&injected).is_none());
    }
}
