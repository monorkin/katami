//! What a program built on katami decides for it.
//!
//! On its own, katami keeps the person's memory in their data folder and
//! works with their Claude login. A program that embeds it — one that has a
//! memory of its own and works as a login of its own — says so here, once,
//! before it does anything else. Every path in `paths` looks here first.
//! Nothing is read from the environment for this: a program that has to set
//! variables in its own process to reach a crate it links is being talked
//! to through the wrong door, and the variables leak into everything it
//! starts.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Default, Clone)]
pub struct Settings {
    /// Where the memory store and everything around it lives, instead of
    /// the person's katami folder.
    pub data_dir: Option<PathBuf>,
    /// Which Claude Code config folder — which login — sessions run under,
    /// instead of `CLAUDE_CONFIG_DIR` or `~/.claude`.
    pub claude_config_dir: Option<PathBuf>,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Once, before anything reads a path. A second call is a programming error
/// and says so, rather than quietly leaving the first in place.
pub fn configure(settings: Settings) {
    if SETTINGS.set(settings).is_err() {
        panic!("katami was configured twice");
    }
}

pub fn settings() -> &'static Settings {
    static ON_ITS_OWN: Settings = Settings { data_dir: None, claude_config_dir: None };
    SETTINGS.get().unwrap_or(&ON_ITS_OWN)
}
