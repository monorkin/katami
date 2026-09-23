//! Where agent keeps its state, and where Claude Code keeps its.
//!
//! The memory store, models, overlays, and logs all live under one shared
//! data directory — memories belong to the person, not to whichever account
//! or config dir a session happened to run under. Claude's config dir still
//! matters for two things: which account's auth a background `claude -p`
//! run uses, and where generated skills get materialized.

use anyhow::{Context, Result};
use std::env;
use std::path::PathBuf;

use crate::settings;

pub fn claude_config_home() -> PathBuf {
    if let Some(dir) = &settings::settings().claude_config_dir {
        dir.clone()
    } else if let Some(dir) = env::var_os("CLAUDE_CONFIG_DIR") {
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

/// A program built on katami that keeps a memory of its own rather than
/// sharing the person's says where in `settings`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = &settings::settings().data_dir {
        return dir.clone();
    }

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

/// A name for something one supervised session owns — its hook socket, its
/// settings overlay. The process id alone isn't enough: a program built on
/// katami supervises several sessions at once from one process, and they
/// would take each other's socket and delete each other's overlay.
pub fn name_for_one_session() -> String {
    static SESSIONS_SO_FAR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let session = SESSIONS_SO_FAR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{session}", std::process::id())
}

/// Katami's own path, for the hooks and helpers it names. A program that was
/// rebuilt while it ran gets its path back from the kernel with " (deleted)"
/// on the end, which is not a path anything can run; the binary now at that
/// path is the new build of the same program, and it answers as well.
pub fn own_binary() -> Result<PathBuf> {
    let binary = env::current_exe().context("could not determine the katami binary path")?;
    Ok(the_build_that_is_there(binary))
}

fn the_build_that_is_there(binary: PathBuf) -> PathBuf {
    let named = binary.to_string_lossy().into_owned();
    match named.strip_suffix(" (deleted)").map(PathBuf::from) {
        Some(replaced) if replaced.is_file() => replaced,
        _ => binary,
    }
}

fn home() -> PathBuf {
    dirs::home_dir().expect("could not determine the home directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_replaced_while_it_ran_is_named_by_the_build_that_took_its_place() {
        let directory = env::temp_dir().join(format!("katami-own-binary-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let rebuilt = directory.join("katami");
        std::fs::write(&rebuilt, "#!/bin/sh\n").unwrap();

        let deleted = PathBuf::from(format!("{} (deleted)", rebuilt.display()));
        assert_eq!(the_build_that_is_there(deleted), rebuilt, "the path without the suffix holds the new build");

        let gone = PathBuf::from(format!("{}/nowhere (deleted)", directory.display()));
        assert_eq!(the_build_that_is_there(gone.clone()), gone, "with nothing there, nothing is made up");
        assert_eq!(the_build_that_is_there(rebuilt.clone()), rebuilt);
        assert!(own_binary().unwrap().is_file(), "and the running test binary is itself");

        std::fs::remove_dir_all(directory).unwrap();
    }
}
