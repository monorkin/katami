use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let payload = serde_json::to_string_pretty(value)?;
    write_atomically(path, &payload)
}

pub fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(parent)?;

    let temporary = path.with_extension("tmp");
    fs::write(&temporary, contents)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temporary, path).with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

pub fn read_json(path: &Path) -> Result<serde_json::Value> {
    let contents =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("could not parse {}", path.display()))
}
