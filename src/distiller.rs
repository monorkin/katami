//! The one place a background job talks to a model.
//!
//! The reviewer and curator both work the same way: hand a headless
//! `claude -p --model haiku` a prompt and some input, get strict JSON back,
//! deserialize it into a typed contract. The session's own CLAUDE_CONFIG_DIR
//! rides along so the right account's auth pays for the call, and the hook
//! socket is stripped so the headless run's hooks no-op instead of relaying
//! into the live session.

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::hook_protocol;

pub fn ask<T: DeserializeOwned>(prompt: &str, input: &str, config_dir: &Path) -> Result<T> {
    let mut command = Command::new("claude");
    command
        .args(["-p", prompt, "--model", "haiku", "--output-format", "json"])
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .env_remove(hook_protocol::SOCKET_ENV_VAR)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = command
        .spawn()
        .context("could not launch claude — is it on your PATH?")?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("claude exited with {}", output.status);
    }

    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("claude's --output-format json envelope did not parse")?;
    let result = envelope["result"]
        .as_str()
        .context("claude's output carried no result field")?;
    serde_json::from_str(strip_fences(result)).context("the reply was not the expected JSON")
}

fn strip_fences(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let body = rest.split_once('\n').map(|it| it.1).unwrap_or(rest);
    body.strip_suffix("```").unwrap_or(body).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fenced_json_is_unwrapped() {
        assert_eq!(strip_fences("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_fences("{\"a\":1}"), "{\"a\":1}");
    }
}
