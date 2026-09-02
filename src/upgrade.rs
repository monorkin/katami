//! `katami upgrade`: move a mise install to the latest release.
//!
//! katami ships only through mise, so this handles exactly that install
//! method: it checks GitHub for a newer release and, when the running binary
//! is one mise put there, runs `mise use` to pull the new version and confirms
//! the swap landed. Any other install (a hand-copied binary, a `cargo install`
//! build) is told how to update itself rather than being touched. Upgrades
//! only ever move forward.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

const CURRENT: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "monorkin/katami";
const MISE_TOOL: &str = "github:monorkin/katami";
const USER_AGENT: &str = concat!("katami/", env!("CARGO_PKG_VERSION"));

pub fn run(requested: Option<&str>) -> Result<()> {
    let requested = requested.map(|it| it.trim_start_matches('v'));
    println!("Current version: {CURRENT}");

    let latest = match requested {
        Some(version) => fetch_release(&format!("tags/v{version}"))
            .with_context(|| format!("could not fetch release v{version}"))?,
        None => fetch_release("latest").context("could not check for updates")?,
    };

    if !is_newer(&latest, CURRENT) {
        println!("Already up to date ({CURRENT})");
        return Ok(());
    }
    println!("Update available: {CURRENT} → {latest}");

    match mise_install()? {
        Some(install) => upgrade_via_mise(&install, &latest),
        None => bail!(
            "update available ({CURRENT} → {latest}), but this binary isn't a mise install — \
             reinstall with `mise use -g {MISE_TOOL}@{latest}`, or download it from \
             https://github.com/{REPO}/releases/tag/v{latest}"
        ),
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

/// The release version behind a GitHub API path — `latest` or `tags/vX.Y.Z`.
fn fetch_release(path: &str) -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/{path}");
    let release: Release = ureq::get(&url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("could not reach {url}"))?
        .body_mut()
        .read_json()
        .context("GitHub's release response did not parse")?;

    let version = release.tag_name.trim_start_matches('v').to_string();
    if version.is_empty() {
        bail!("the release carries no version tag");
    }
    Ok(version)
}

/// A dotted version compare that keeps a prerelease (`0.2.0-rc.1`) below its
/// release. Enough for the tags katami cuts; not a full semver ordering.
fn is_newer(candidate: &str, current: &str) -> bool {
    version_key(candidate) > version_key(current)
}

fn version_key(version: &str) -> (u64, u64, u64, u8) {
    let core = version.split('-').next().unwrap_or(version);
    let mut parts = core.split('.').map(|part| part.parse().unwrap_or(0));
    let released = if version.contains('-') { 0 } else { 1 };
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        released,
    )
}

struct MiseInstall {
    mise: PathBuf,
    config_path: Option<String>,
}

/// The running binary is a mise install when it sits in mise's release-backend
/// layout: `…/installs/github-monorkin-katami/<version>/katami`. When it is,
/// the config that selected it is resolved so the upgrade edits that file
/// rather than guessing at the global config.
fn mise_install() -> Result<Option<MiseInstall>> {
    let Ok(exe) = std::env::current_exe() else {
        return Ok(None);
    };
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    if !is_mise_layout(&exe) {
        return Ok(None);
    }
    let Ok(mise) = which("mise") else {
        bail!("this looks like a mise install but `mise` isn't on PATH — put it back and retry");
    };

    Ok(Some(MiseInstall {
        config_path: mise_config_path(&mise),
        mise,
    }))
}

fn is_mise_layout(exe: &Path) -> bool {
    let components: Vec<&str> = exe
        .components()
        .filter_map(|it| it.as_os_str().to_str())
        .collect();
    components
        .windows(2)
        .any(|pair| pair == ["installs", "github-monorkin-katami"])
}

fn mise_config_path(mise: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Tool {
        version: String,
        active: bool,
        source: Source,
    }
    #[derive(Deserialize)]
    struct Source {
        path: String,
    }

    let output = Command::new(mise)
        .args(["ls", "--json", MISE_TOOL])
        .output()
        .ok()?;
    let tools: Vec<Tool> = serde_json::from_slice(&output.stdout).ok()?;
    tools
        .into_iter()
        .find(|tool| tool.active && tool.version == CURRENT && Path::new(&tool.source.path).is_absolute())
        .map(|tool| tool.source.path)
}

fn upgrade_via_mise(install: &MiseInstall, latest: &str) -> Result<()> {
    println!("Upgrading via mise…");
    let spec = format!("{MISE_TOOL}@{latest}");

    let mut command = Command::new(&install.mise);
    match &install.config_path {
        Some(path) => command.args(["use", "--path", path, &spec]),
        None => command.args(["use", "-g", &spec]),
    };
    let status = command
        .status()
        .with_context(|| format!("could not run mise — retry manually: {}", use_hint(install, latest)))?;
    if !status.success() {
        bail!("mise upgrade failed — retry manually: {}", use_hint(install, latest));
    }

    confirm(install, latest)
}

/// A managed upgrade isn't done until the newly selected binary reports the
/// version we asked for — mise can succeed while an older binary stays active.
fn confirm(install: &MiseInstall, latest: &str) -> Result<()> {
    let path = Command::new(&install.mise)
        .args(["which", "katami"])
        .output()
        .ok()
        .filter(|it| it.status.success())
        .map(|it| String::from_utf8_lossy(&it.stdout).trim().to_string())
        .filter(|it| !it.is_empty());

    let Some(path) = path else {
        bail!("mise reported success but the new binary couldn't be located — run `katami version` to confirm");
    };

    let reported = Command::new(&path)
        .arg("--version")
        .output()
        .ok()
        .and_then(|it| String::from_utf8(it.stdout).ok())
        .and_then(|it| it.split_whitespace().last().map(str::to_string));

    match reported {
        Some(version) if version == latest => {
            println!("Upgraded {CURRENT} → {version}");
            Ok(())
        }
        Some(version) => bail!(
            "mise finished but katami still reports {version} (expected {latest}) — retry: {}",
            use_hint(install, latest)
        ),
        None => bail!("mise finished but the new binary's version couldn't be read — run `katami version` to confirm"),
    }
}

fn use_hint(install: &MiseInstall, latest: &str) -> String {
    match &install.config_path {
        Some(path) => format!("mise use --path {path} {MISE_TOOL}@{latest}"),
        None => format!("mise use -g {MISE_TOOL}@{latest}"),
    }
}

fn which(program: &str) -> Result<PathBuf> {
    let path = Command::new("sh")
        .args(["-c", &format!("command -v {program}")])
        .output()?;
    let resolved = String::from_utf8_lossy(&path.stdout).trim().to_string();
    if resolved.is_empty() {
        bail!("{program} not found on PATH");
    }
    Ok(PathBuf::from(resolved))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_moves_forward_and_ranks_prereleases_below_releases() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(is_newer("0.2.0", "0.2.0-rc.1"));
        assert!(is_newer("0.2.0-rc.1", "0.1.0"));
        assert!(!is_newer("0.2.0-rc.1", "0.2.0"));
    }

    #[test]
    fn mise_layout_only_matches_our_install_tree() {
        assert!(is_mise_layout(Path::new(
            "/home/x/.local/share/mise/installs/github-monorkin-katami/0.1.0/katami"
        )));
        assert!(!is_mise_layout(Path::new("/usr/bin/katami")));
        assert!(!is_mise_layout(Path::new(
            "/home/x/.local/share/mise/installs/github-monorkin-ax/1.0/ax"
        )));
    }
}
