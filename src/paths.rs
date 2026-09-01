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

pub fn data_dir() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_DATA_HOME").filter(|it| !it.is_empty()) {
        PathBuf::from(dir).join("agent")
    } else {
        home().join(".local/share/agent")
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
        PathBuf::from(dir).join("agent")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/agent-{uid}"))
    }
}

fn home() -> PathBuf {
    dirs::home_dir().expect("could not determine the home directory")
}
