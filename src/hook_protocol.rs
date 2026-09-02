//! The wire between a hook process and the supervisor.
//!
//! One connection per hook call: the client (or a tool's relay) sends a
//! single line of JSON and reads a single line back — the supervisor's
//! canonical reply, which each edge formats into its tool's native shape.
//! Newline-delimited JSON over a unix socket keeps every side trivial and
//! testable; there's no framing to get wrong beyond "one line each way".
//!
//! A frame names the `tool` it came from and, for Claude, the hook process's
//! effective `CLAUDE_CONFIG_DIR`. The supervisor can't know pre-launch which
//! config dir a wrapper like `ax run` will pick, but hooks are children of
//! claude and inherit its environment — so the frame is where that knowledge
//! crosses over. Other tools don't carry it: they don't have per-account
//! config dirs the reviewer needs.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Where a relay finds the supervisor's socket — part of the wire contract,
/// and doubling as the recursion guard and the arming gate: a process without
/// it (a headless reviewer run, an unsupervised session) relays nothing.
pub const SOCKET_ENV_VAR: &str = "AGENT_HOOK_SOCKET";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    #[default]
    Claude,
    Codex,
    Pi,
    Opencode,
}

impl Tool {
    pub fn parse(name: &str) -> Option<Tool> {
        match name {
            "claude" => Some(Tool::Claude),
            "codex" => Some(Tool::Codex),
            "pi" => Some(Tool::Pi),
            "opencode" => Some(Tool::Opencode),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
            Tool::Pi => "pi",
            Tool::Opencode => "opencode",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct HookRequest {
    pub event: String,
    #[serde(default)]
    pub tool: Tool,
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
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
            tool: Tool::Codex,
            config_dir: None,
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
        assert_eq!(parsed.tool, Tool::Codex);
        assert_eq!(parsed.payload["prompt"], "find my notes");
    }

    #[test]
    fn old_frames_without_tool_parse_as_claude() {
        let wire = concat!(
            r#"{"event":"Stop","config_dir":"/home/someone/.claude","#,
            r#""payload":{"session_id":"s1"}}"#,
            "\n"
        );
        let parsed = read_frame(&mut BufReader::new(wire.as_bytes())).unwrap();

        assert_eq!(parsed.tool, Tool::Claude);
        assert_eq!(parsed.config_dir, Some(PathBuf::from("/home/someone/.claude")));
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
