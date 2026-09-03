//! The one place a background job talks to a model.
//!
//! The reviewer and curator both work the same way: hand a headless
//! `claude -p --model haiku` a prompt and some input, get strict JSON back,
//! deserialize it into a typed contract. The session's own CLAUDE_CONFIG_DIR
//! rides along so the right account's auth pays for the call, and the hook
//! socket is stripped so the headless run's hooks no-op instead of relaying
//! into the live session.
//!
//! Haiku occasionally answers in prose or bends the schema. Rather than burn a
//! whole retry with backoff on that, the same session is resumed with the
//! parse error and asked for the JSON again, up to three tries in all — it
//! already has the input, so a correction is cheap and usually lands.

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::hook_protocol;

const CORRECTIONS: usize = 2;

/// `check` is the caller's semantic validation — evidence that exists, ids it
/// was shown, classes it knows. A failure there is fed back to the model just
/// like a parse failure, since both mean the same thing: try again.
pub fn ask<T: DeserializeOwned>(
    prompt: &str,
    input: &str,
    config_dir: &Path,
    check: impl Fn(&T) -> Result<()>,
) -> Result<T> {
    let reply = run(prompt, input, None, config_dir)?;
    let mut outcome = parse(&reply.result).and_then(|it| accept(it, &check));

    for _ in 0..CORRECTIONS {
        let Err(error) = &outcome else { break };
        let correction = format!(
            "That reply could not be used: {error:#}. Reply again with ONLY the JSON object described in the instructions — no prose, no code fences — with that problem fixed. Drop an item rather than invent evidence or ids for it."
        );
        let retried = run(&correction, "", Some(&reply.session_id), config_dir)?;
        outcome = parse(&retried.result).and_then(|it| accept(it, &check));
    }
    outcome
}

fn accept<T>(value: T, check: &impl Fn(&T) -> Result<()>) -> Result<T> {
    check(&value)?;
    Ok(value)
}

struct Reply {
    session_id: String,
    result: String,
}

fn run(prompt: &str, input: &str, resume: Option<&str>, config_dir: &Path) -> Result<Reply> {
    let mut command = Command::new("claude");
    command
        .args(["-p", prompt, "--model", "haiku", "--output-format", "json"])
        .env("CLAUDE_CONFIG_DIR", config_dir)
        .env_remove(hook_protocol::SOCKET_ENV_VAR)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(session_id) = resume {
        command.args(["--resume", session_id]);
    }

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
    let session_id = envelope["session_id"]
        .as_str()
        .context("claude's output carried no session_id field")?;
    Ok(Reply { session_id: session_id.to_string(), result: result.to_string() })
}

fn parse<T: DeserializeOwned>(result: &str) -> Result<T> {
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
