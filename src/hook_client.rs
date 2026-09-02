//! `katami hook <tool> <event>`: the thin client claude and codex run.
//!
//! It relays: hook JSON from stdin to the supervisor's socket, and the
//! supervisor's canonical reply back out — formatted into the tool's native
//! hook-output shape. claude and codex both speak `hookSpecificOutput`, so
//! one Rust client serves both; pi and opencode consume the canonical reply
//! directly in their TS relays and never run this.
//!
//! Every failure path prints `{}` and exits 0 — a hook must never wound the
//! session it serves. That includes the missing-socket case, which is also
//! the arming gate: a headless reviewer or an unsupervised session has no
//! socket env, so its hooks no-op instantly.

use anyhow::Result;
use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::hook_protocol::{self, HookRequest, SOCKET_ENV_VAR, Tool};
use crate::paths;

const TIMEOUT: Duration = Duration::from_millis(1500);

pub fn run(tool: Tool, event: &str) -> Result<()> {
    let reply = relay(tool, event).unwrap_or_else(|_| serde_json::json!({}));
    println!("{}", format_reply(tool, event, &reply));
    Ok(())
}

fn relay(tool: Tool, event: &str) -> Result<serde_json::Value> {
    let socket_path = std::env::var(SOCKET_ENV_VAR)?;

    let mut payload = String::new();
    std::io::stdin().read_to_string(&mut payload)?;

    let request = HookRequest {
        event: event.to_string(),
        tool,
        config_dir: match tool {
            Tool::Claude => Some(paths::claude_config_home()),
            _ => None,
        },
        payload: serde_json::from_str(&payload)?,
    };

    let mut stream = UnixStream::connect(&socket_path)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    hook_protocol::write_frame(&mut stream, &request)?;
    hook_protocol::read_reply(&mut BufReader::new(&mut stream))
}

/// Turns the canonical `{"context": ...}` reply into the tool's native shape.
/// An empty or absent context becomes `{}` — nothing to inject. Claude wraps
/// the context in `<katami-memory>` sentinels so its transcript parser can
/// strip its own injection back out; codex needs no sentinel because its
/// parser filters injected context structurally by role and kind.
fn format_reply(tool: Tool, event: &str, reply: &serde_json::Value) -> serde_json::Value {
    let Some(context) = reply["context"].as_str().filter(|it| !it.is_empty()) else {
        return serde_json::json!({});
    };
    let context = match tool {
        Tool::Claude => format!("<katami-memory>\n{context}\n</katami-memory>"),
        _ => context.to_string(),
    };
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": context
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_wraps_injected_context_in_sentinels() {
        let reply = serde_json::json!({ "context": "a memory" });
        let out = format_reply(Tool::Claude, "UserPromptSubmit", &reply);
        let injected = out["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
        assert!(injected.starts_with("<katami-memory>"));
        assert!(injected.contains("a memory"));
    }

    #[test]
    fn codex_gets_bare_context_no_sentinels() {
        let reply = serde_json::json!({ "context": "a memory" });
        let out = format_reply(Tool::Codex, "SessionStart", &reply);
        assert_eq!(
            out["hookSpecificOutput"]["additionalContext"],
            "a memory"
        );
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    }

    #[test]
    fn empty_context_is_an_empty_object() {
        assert_eq!(format_reply(Tool::Claude, "Stop", &serde_json::json!({})), serde_json::json!({}));
        assert_eq!(
            format_reply(Tool::Codex, "Stop", &serde_json::json!({ "context": "" })),
            serde_json::json!({})
        );
    }
}
