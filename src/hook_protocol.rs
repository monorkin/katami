//! The wire between a hook process and the supervisor.
//!
//! One connection per hook call: the client sends a single line of JSON and
//! reads a single line back — the exact body the hook prints to Claude Code.
//! Newline-delimited JSON over a unix socket keeps both sides trivial and
//! testable; there's no framing to get wrong beyond "one line each way".
//!
//! Every frame carries the hook process's effective `CLAUDE_CONFIG_DIR`. The
//! supervisor can't know pre-launch which config dir a wrapper like `ax run`
//! will pick, but hooks are children of claude and inherit its environment —
//! so the frame is where that knowledge crosses over.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub struct HookRequest {
    pub event: String,
    pub config_dir: PathBuf,
    pub payload: serde_json::Value,
}

pub fn write_frame(stream: &mut impl Write, request: &HookRequest) -> Result<()> {
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .context("could not send the hook frame")
}

pub fn read_frame(stream: &mut impl BufRead) -> Result<HookRequest> {
    let line = read_line(stream)?;
    serde_json::from_str(&line).context("could not parse the hook frame")
}

pub fn write_reply(stream: &mut impl Write, reply: &serde_json::Value) -> Result<()> {
    let mut line = serde_json::to_string(reply)?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .context("could not send the hook reply")
}

pub fn read_reply(stream: &mut impl BufRead) -> Result<serde_json::Value> {
    let line = read_line(stream)?;
    serde_json::from_str(&line).context("could not parse the hook reply")
}

fn read_line(stream: &mut impl BufRead) -> Result<String> {
    let mut line = String::new();
    stream
        .read_line(&mut line)
        .context("could not read from the hook socket")?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::BufReader;

    #[test]
    fn frames_round_trip() {
        let request = HookRequest {
            event: "UserPromptSubmit".into(),
            config_dir: "/home/someone/.claude".into(),
            payload: json!({
                "session_id": "abc123",
                "transcript_path": "/tmp/transcript.jsonl",
                "cwd": "/home/someone/project",
                "hook_event_name": "UserPromptSubmit",
                "prompt": "find my notes"
            }),
        };

        let mut wire = Vec::new();
        write_frame(&mut wire, &request).unwrap();
        let parsed = read_frame(&mut BufReader::new(wire.as_slice())).unwrap();

        assert_eq!(parsed.event, "UserPromptSubmit");
        assert_eq!(parsed.config_dir, PathBuf::from("/home/someone/.claude"));
        assert_eq!(parsed.payload["prompt"], "find my notes");
    }

    #[test]
    fn replies_round_trip() {
        let reply = json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": "relevant memory"
            }
        });

        let mut wire = Vec::new();
        write_reply(&mut wire, &reply).unwrap();
        let parsed = read_reply(&mut BufReader::new(wire.as_slice())).unwrap();

        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            "relevant memory"
        );
    }

    #[test]
    fn post_tool_use_payload_fields_parse() {
        let payload = json!({
            "session_id": "abc123",
            "transcript_path": "/tmp/transcript.jsonl",
            "cwd": "/home/someone/project",
            "hook_event_name": "PostToolUse",
            "tool_name": "Skill",
            "tool_input": { "skill": "hey" },
            "tool_use_id": "toolu_123"
        });

        assert_eq!(payload["tool_name"], "Skill");
        assert_eq!(payload["tool_input"]["skill"], "hey");
    }
}
