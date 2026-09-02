//! `agent hook <event>`: the thin client Claude Code actually runs.
//!
//! All it does is relay: hook JSON from stdin to the supervisor's socket,
//! the supervisor's reply to stdout. Every failure path prints `{}` and
//! exits 0 — a hook must never wound the session it serves. That includes
//! the missing-socket case, which is also what makes headless reviewer and
//! curator runs safe: they get no socket env, so their hooks no-op instantly.

use anyhow::Result;
use std::io::{BufReader, Read};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use crate::hook_protocol::{self, HookRequest, SOCKET_ENV_VAR};
use crate::paths;

const TIMEOUT: Duration = Duration::from_millis(1500);

pub fn run(event: &str) -> Result<()> {
    let reply = relay(event).unwrap_or_else(|_| serde_json::json!({}));
    println!("{reply}");
    Ok(())
}

fn relay(event: &str) -> Result<serde_json::Value> {
    let socket_path = std::env::var(SOCKET_ENV_VAR)?;

    let mut payload = String::new();
    std::io::stdin().read_to_string(&mut payload)?;

    let request = HookRequest {
        event: event.to_string(),
        config_dir: paths::claude_config_home(),
        payload: serde_json::from_str(&payload)?,
    };

    let mut stream = UnixStream::connect(&socket_path)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    hook_protocol::write_frame(&mut stream, &request)?;
    hook_protocol::read_reply(&mut BufReader::new(&mut stream))
}
