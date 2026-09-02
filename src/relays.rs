//! Installing the memory relays into the other coding tools.
//!
//! Every tool gets its relay written into its global config once, at launch
//! and on demand — persistent, not per-session, because a supervised session
//! can shell out to another tool mid-run and that inner tool must relay to the
//! same supervisor. The relays gate on `AGENT_HOOK_SOCKET`, so an installed
//! relay is inert outside a supervised session. Everything is additive and
//! marked as agent-managed: pi and opencode get one materialized file each,
//! codex gets our hook entries merged into its `hooks.json` with the user's
//! own entries preserved.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::fsutil;
use crate::paths;

const PI_RELAY: &str = include_str!("relays/agent-memory.pi.ts");
const OPENCODE_RELAY: &str = include_str!("relays/agent-memory.opencode.js");

/// The codex hook events we register (no PostToolUse — skill tracking is a
/// claude-only concept) and their timeouts in seconds. SessionEnd's hard cap
/// is 3s; our client gives up at 1.5s, safely inside.
const CODEX_EVENTS: [(&str, u64); 4] = [
    ("SessionStart", 5),
    ("UserPromptSubmit", 3),
    ("Stop", 3),
    ("SessionEnd", 3),
];

pub struct Installed {
    pub tool: &'static str,
    pub path: PathBuf,
    pub changed: bool,
}

/// Installs every relay, quietly, at launch. Prints a trust reminder only if
/// codex's hooks changed — codex skips untrusted hooks until the user runs
/// `/hooks` once.
pub fn install_all() -> Result<()> {
    let agent = std::env::current_exe().context("could not determine the agent binary path")?;
    let installed = install(&agent)?;
    if installed.iter().any(|it| it.tool == "codex" && it.changed) {
        eprintln!(
            "note: codex memory hooks were installed — run `/hooks` inside codex once and trust the agent entries, or they stay skipped."
        );
    }
    Ok(())
}

pub fn install_command() -> Result<()> {
    let agent = std::env::current_exe().context("could not determine the agent binary path")?;
    for entry in install(&agent)? {
        let state = if entry.changed { "installed" } else { "already current" };
        println!("{:<9} {state:<16} {}", entry.tool, entry.path.display());
    }
    println!("\nIf codex was updated, run `/hooks` inside codex once to trust the agent entries.");
    Ok(())
}

pub fn status_command() -> Result<()> {
    println!("{:<9} {:<12} path", "tool", "state");
    for (tool, path, current) in current_states()? {
        let state = if current { "installed" } else { "missing" };
        println!("{tool:<9} {state:<12} {}", path.display());
    }
    Ok(())
}

fn install(agent: &Path) -> Result<Vec<Installed>> {
    Ok(vec![
        install_file("pi", &pi_path(), &pi_contents())?,
        install_file("opencode", &opencode_path(), OPENCODE_RELAY)?,
        install_codex(agent)?,
    ])
}

fn install_file(tool: &'static str, path: &Path, contents: &str) -> Result<Installed> {
    let changed = std::fs::read_to_string(path).ok().as_deref() != Some(contents);
    if changed {
        fsutil::write_atomically(path, contents)?;
    }
    Ok(Installed { tool, path: path.to_path_buf(), changed })
}

/// Merges our command hooks into codex's `hooks.json`, dropping any earlier
/// agent entries (they name a `hook codex` command) and keeping everything
/// else — the user's own hooks survive verbatim.
fn install_codex(agent: &Path) -> Result<Installed> {
    let path = codex_hooks_path();
    let mut config = match fsutil::read_json(&path) {
        Ok(value) if value.is_object() => value,
        _ => json!({}),
    };
    let before = config.clone();
    merge_codex_hooks(&mut config, agent);

    let changed = config != before;
    if changed {
        fsutil::write_json_atomically(&path, &config)?;
    }
    Ok(Installed { tool: "codex", path, changed })
}

fn merge_codex_hooks(config: &mut Value, agent: &Path) {
    let root = config.as_object_mut().expect("codex config is an object");
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }

    let hooks = hooks.as_object_mut().expect("hooks were just made an object");
    for (event, timeout) in CODEX_EVENTS {
        let command = format!("{} hook codex {event}", agent.display());
        let ours = json!({ "hooks": [{ "type": "command", "command": command, "timeout": timeout }] });

        let groups = hooks.entry(event).or_insert_with(|| json!([]));
        if !groups.is_array() {
            *groups = json!([]);
        }
        let groups = groups.as_array_mut().unwrap();
        groups.retain(|group| !is_agent_group(group));
        groups.push(ours);
    }
}

/// Any group whose command relays codex to us — a `hook codex` invocation,
/// regardless of the binary path, so an entry pointing at an old agent binary
/// gets replaced.
fn is_agent_group(group: &Value) -> bool {
    group["hooks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|hook| hook["command"].as_str())
        .any(|command| command.contains(" hook codex "))
}

fn current_states() -> Result<Vec<(&'static str, PathBuf, bool)>> {
    let agent = std::env::current_exe().context("could not determine the agent binary path")?;
    let pi = pi_path();
    let opencode = opencode_path();
    let codex = codex_hooks_path();
    Ok(vec![
        ("pi", pi.clone(), std::fs::read_to_string(&pi).ok().as_deref() == Some(pi_contents().as_str())),
        ("opencode", opencode.clone(), std::fs::read_to_string(&opencode).ok().as_deref() == Some(OPENCODE_RELAY)),
        ("codex", codex.clone(), codex_installed(&codex, &agent)),
    ])
}

fn codex_installed(path: &Path, _agent: &Path) -> bool {
    let Ok(config) = fsutil::read_json(path) else {
        return false;
    };
    CODEX_EVENTS.iter().all(|(event, _)| {
        config["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .any(is_agent_group)
    })
}

fn pi_contents() -> String {
    PI_RELAY.to_string()
}

fn pi_path() -> PathBuf {
    paths::pi_extensions_dir().join("agent-memory.ts")
}

fn opencode_path() -> PathBuf {
    paths::opencode_plugins_dir().join("agent-memory.js")
}

fn codex_hooks_path() -> PathBuf {
    paths::codex_home().join("hooks.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_merge_preserves_user_hooks_and_is_idempotent() {
        let agent = Path::new("/usr/bin/agent");
        let mut config = json!({
            "hooks": {
                "SessionStart": [
                    { "hooks": [{ "type": "command", "command": "bash /home/x/herdr.sh session" }] }
                ]
            }
        });

        merge_codex_hooks(&mut config, agent);
        let after_first = config.clone();
        merge_codex_hooks(&mut config, agent);
        assert_eq!(config, after_first, "merging twice changes nothing");

        let session_start = config["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 2, "user's herdr hook is kept");
        assert!(session_start.iter().any(|group| group["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("herdr.sh")));
        assert!(session_start.iter().any(|group| group["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("hook codex SessionStart")));
        assert!(config["hooks"]["Stop"].is_array());
        assert!(config["hooks"].get("PostToolUse").is_none());
    }

    #[test]
    fn a_stale_agent_entry_is_replaced_not_duplicated() {
        let mut config = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "/old/path/agent hook codex Stop", "timeout": 3 }] }
                ]
            }
        });
        merge_codex_hooks(&mut config, Path::new("/new/agent"));
        let stop = config["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1);
        assert!(stop[0]["hooks"][0]["command"].as_str().unwrap().starts_with("/new/agent"));
    }
}
