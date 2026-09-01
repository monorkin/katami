//! The settings overlay: how agent's hooks reach Claude Code without touching
//! the user's configuration.
//!
//! Claude Code merges settings from every source — user, project, local,
//! managed, and a `--settings` file — and hooks merge across levels rather
//! than replacing each other. So a per-launch overlay file passed via
//! `--settings` adds agent's hooks for exactly one session and leaves every
//! other settings file alone. If the user forwarded their own `--settings`,
//! the two files are merged into the overlay instead of passing the flag
//! twice, since claude's behavior for a repeated flag is undocumented.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use crate::fsutil;
use crate::paths;

const EVENTS: [(&str, Option<&str>, u32); 6] = [
    ("SessionStart", None, 5),
    ("UserPromptSubmit", None, 3),
    ("PostToolUse", Some("Skill|Read"), 2),
    ("Stop", None, 2),
    ("PreCompact", None, 2),
    ("SessionEnd", None, 1),
];

pub fn write(user_settings: Option<&Path>) -> Result<PathBuf> {
    let agent_binary =
        std::env::current_exe().context("could not determine the agent binary path")?;

    let mut settings = match user_settings {
        Some(path) => fsutil::read_json(path)?,
        None => json!({}),
    };
    merge_hooks(&mut settings, &hooks(&agent_binary));

    let path = paths::overlays_dir().join(format!("{}.json", std::process::id()));
    fsutil::write_json_atomically(&path, &settings)?;
    Ok(path)
}

pub fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn hooks(agent_binary: &Path) -> Value {
    let mut hooks = serde_json::Map::new();
    for (event, matcher, timeout) in EVENTS {
        let command = format!("{} hook {event}", agent_binary.display());
        let mut entry = serde_json::Map::new();
        if let Some(matcher) = matcher {
            entry.insert("matcher".into(), json!(matcher));
        }
        entry.insert(
            "hooks".into(),
            json!([{ "type": "command", "command": command, "timeout": timeout }]),
        );
        hooks.insert(event.into(), json!([Value::Object(entry)]));
    }
    Value::Object(hooks)
}

fn merge_hooks(settings: &mut Value, ours: &Value) {
    let root = settings
        .as_object_mut()
        .expect("settings overlay is always a JSON object");
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));

    for (event, entries) in ours.as_object().expect("hooks are always an object") {
        match hooks.get_mut(event) {
            Some(Value::Array(existing)) => {
                existing.extend(entries.as_array().expect("entries are always an array").clone());
            }
            _ => {
                hooks[event] = entries.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_cover_every_event_with_timeouts() {
        let hooks = hooks(Path::new("/usr/bin/agent"));

        for (event, matcher, timeout) in EVENTS {
            let entry = &hooks[event][0];
            let hook = &entry["hooks"][0];
            assert_eq!(hook["type"], "command");
            assert_eq!(hook["command"], format!("/usr/bin/agent hook {event}"));
            assert_eq!(hook["timeout"], timeout);
            match matcher {
                Some(matcher) => assert_eq!(entry["matcher"], matcher),
                None => assert!(entry.get("matcher").is_none()),
            }
        }
    }

    #[test]
    fn merging_keeps_the_users_hooks() {
        let mut settings = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": "notify-send done" }] }]
            }
        });

        merge_hooks(&mut settings, &hooks(Path::new("/usr/bin/agent")));

        assert_eq!(settings["model"], "opus");
        let stop = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "notify-send done");
        assert_eq!(stop[1]["hooks"][0]["command"], "/usr/bin/agent hook Stop");
        assert!(settings["hooks"]["UserPromptSubmit"].is_array());
    }
}
