//! Remembering which claude config dir a launch ends up using.
//!
//! When the wrapped command is a plain `claude`, the config dir is knowable
//! before launch. When it's a wrapper like `ax run` that picks its own
//! CLAUDE_CONFIG_DIR mid-flight, agent only learns the real dir from the
//! first hook frame — too late for pre-launch work like materializing
//! skills. So each launch key (cwd + claude command) records the config dir
//! it last resolved to, and the next launch under the same key starts with
//! that knowledge. First-ever launches simply skip pre-launch work.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::flock;
use crate::fsutil;
use crate::paths;

#[derive(Serialize, Deserialize, Default)]
struct LaunchesFile {
    schema_version: u32,
    launches: BTreeMap<String, Launch>,
}

#[derive(Serialize, Deserialize)]
struct Launch {
    config_dir: PathBuf,
    updated: String,
}

pub fn key(cwd: &Path, claude_cmd: &str) -> String {
    format!("{}\u{1f}{claude_cmd}", cwd.display())
}

pub fn config_dir_for(key: &str) -> Option<PathBuf> {
    let file = load().ok()?;
    file.launches.get(key).map(|it| it.config_dir.clone())
}

pub fn record(key: &str, config_dir: &Path) -> Result<()> {
    let patience = std::time::Duration::from_secs(2);
    let Some(_lock) = flock::acquire(&paths::data_dir().join("launches.lock"), patience)? else {
        return Ok(());
    };

    let mut file = load().unwrap_or_default();
    file.schema_version = 1;
    file.launches.insert(
        key.to_string(),
        Launch {
            config_dir: config_dir.to_path_buf(),
            updated: crate::timestamp(),
        },
    );
    fsutil::write_json_atomically(&paths::launches_path(), &file)
}

fn load() -> Result<LaunchesFile> {
    let contents = std::fs::read_to_string(paths::launches_path())?;
    Ok(serde_json::from_str(&contents)?)
}
