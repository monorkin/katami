//! Where agent keeps its state, and where Claude Code keeps its.
//!
//! The memory store, models, overlays, and logs all live under one shared
//! data directory — memories belong to the person, not to whichever account
//! or config dir a session happened to run under. Claude's config dir still
//! matters for two things: which account's auth a background `claude -p`
//! run uses, and where generated skills get materialized.

use std::env;
use std::path::PathBuf;

pub fn claude_config_home() -> PathBuf {
    if let Some(dir) = env::var_os("CLAUDE_CONFIG_DIR") {
        PathBuf::from(dir)
    } else {
        home().join(".claude")
    }
}

/// Where the other coding tools keep the config the relays install into.
pub fn codex_home() -> PathBuf {
    if let Some(dir) = env::var_os("CODEX_HOME") {
        PathBuf::from(dir)
    } else {
        home().join(".codex")
    }
}

pub fn pi_extensions_dir() -> PathBuf {
    pi_agent_dir().join("extensions")
}

fn pi_agent_dir() -> PathBuf {
    if let Some(dir) = env::var_os("PI_CODING_AGENT_DIR").filter(|it| !it.is_empty()) {
        PathBuf::from(dir)
    } else {
        home().join(".pi/agent")
    }
}

pub fn opencode_plugins_dir() -> PathBuf {
    config_home().join("opencode/plugins")
}

pub fn opencode_db() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").filter(|it| !it.is_empty()) {
        PathBuf::from(dir).join("opencode/opencode.db")
    } else {
        home().join(".local/share/opencode/opencode.db")
    }
}

fn config_home() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME").filter(|it| !it.is_empty()) {
        PathBuf::from(dir)
    } else {
        home().join(".config")
    }
}

pub fn data_dir() -> PathBuf {
    let base = |root: PathBuf| root.join("katami");
    // One-time move from the pre-rename location, so an existing store keeps
    // its memories under the new name.
    let dir = if let Some(xdg) = env::var_os("XDG_DATA_HOME").filter(|it| !it.is_empty()) {
        base(PathBuf::from(xdg))
    } else {
        base(home().join(".local/share"))
    };
    migrate_legacy_dir(&dir);
    dir
}

fn migrate_legacy_dir(new_dir: &PathBuf) {
    if new_dir.exists() {
        return;
    }
    let legacy = new_dir.with_file_name("agent");
    if legacy.is_dir() {
        let _ = std::fs::rename(&legacy, new_dir);
    }
}

pub fn memory_dir() -> PathBuf {
    data_dir().join("memory")
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub fn overlays_dir() -> PathBuf {
    data_dir().join("overlays")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn launches_path() -> PathBuf {
    data_dir().join("launches.json")
}

pub fn runtime_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR").filter(|it| !it.is_empty()) {
        PathBuf::from(dir).join("katami")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/katami-{uid}"))
    }
}

fn home() -> PathBuf {
    dirs::home_dir().expect("could not determine the home directory")
}
