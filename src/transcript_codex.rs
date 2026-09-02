//! Parsing codex rollout transcripts.
//!
//! codex writes `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`, one
//! JSON object per line wrapped as `{timestamp, ordinal, type, payload}`. The
//! turns live in `response_item` lines carrying a `message`. codex keeps only
//! `user` and `assistant` roles here — injected context (our own, plus
//! codex's plugin recommendations and environment blocks) arrives as
//! `developer`-role messages, so filtering to those two roles is the
//! structural self-ingestion guard: nothing katami injected ever comes back.

use serde_json::Value;

use crate::transcript::{self, Role, Turn};

pub fn turn_from(record: &Value) -> Option<Turn> {
    if record["type"].as_str()? != "response_item" {
        return None;
    }
    let payload = &record["payload"];
    if payload["type"].as_str()? != "message" {
        return None;
    }

    let role = match payload["role"].as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    transcript::new_turn(role, content_text(&payload["content"]))
}

/// codex content is an array of items; user items carry `input_text`,
/// assistant items `output_text`. Both use a `text` field.
fn content_text(content: &Value) -> String {
    let Value::Array(items) = content else {
        return String::new();
    };
    items
        .iter()
        .filter(|it| matches!(it["type"].as_str(), Some("input_text" | "output_text")))
        .filter_map(|it| it["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(line: &str) -> Value {
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn extracts_user_and_assistant_messages() {
        let user = record(
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"deploy with kamal"}]}}"#,
        );
        let assistant = record(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Deploying now."}]}}"#,
        );
        assert_eq!(turn_from(&user).unwrap().text, "deploy with kamal");
        assert_eq!(turn_from(&assistant).unwrap().text, "Deploying now.");
    }

    #[test]
    fn drops_injected_developer_context_and_non_messages() {
        let developer = record(
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<katami-memory> injected"}]}}"#,
        );
        let reasoning = record(
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[]}}"#,
        );
        let event = record(r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#);
        assert!(turn_from(&developer).is_none());
        assert!(turn_from(&reasoning).is_none());
        assert!(turn_from(&event).is_none());
    }
}
