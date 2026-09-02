//! Composing and launching the wrapped coding tool.
//!
//! `--cmd` names the launcher — a bare `claude`, `codex`, `pi`, `opencode`, or
//! a wrapper like `ax run --account work --` — split on whitespace, so shell
//! quoting isn't interpreted and anything fancier belongs in a wrapper script.
//! Forwarded arguments after `--` are appended verbatim.
//!
//! Every launch first installs the relays into codex, pi, and opencode, so a
//! session that shells out to another tool mid-run relays to this same
//! supervisor. Claude alone needs a per-launch settings overlay and skill
//! materialization; the others carry their integration in their relays.

use anyhow::{Context, Result, bail};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use crate::hook_protocol::Tool;
use crate::launches;
use crate::memory::Memory;
use crate::overlay;
use crate::paths;
use crate::pty;
use crate::relays;
use crate::supervisor;
use crate::virtual_skills;

pub fn run(cmd: &str, claude_args: &[String], exec: bool) -> Result<()> {
    let _ = relays::install_all();

    if exec {
        let mut command = compose(cmd, claude_args.to_vec())?;
        return Err(command.exec())
            .with_context(|| format!("could not launch {cmd} — is it on your PATH?"));
    }

    let launch_key = launches::key(&std::env::current_dir()?, cmd);
    let mut forwarded = claude_args.to_vec();
    let mut overlay_path = None;

    if wraps_claude(cmd) {
        let (args, user_settings) = extract_user_settings(claude_args)?;
        let path = overlay::write(user_settings.as_deref())?;
        forwarded = args;
        forwarded.push("--settings".into());
        forwarded.push(path.display().to_string());
        materialize_skills(cmd, &launch_key);
        overlay_path = Some(path);
    }

    let command = compose(cmd, forwarded)?;
    let code = supervise_or_pipe(command, cmd, launch_key);

    if let Some(path) = &overlay_path {
        overlay::remove(path);
    }
    std::process::exit(code?);
}

fn supervise_or_pipe(mut command: Command, cmd: &str, launch_key: String) -> Result<i32> {
    if pty::is_terminal(libc::STDIN_FILENO) && pty::is_terminal(libc::STDOUT_FILENO) {
        supervisor::supervise(command, launch_key)
    } else {
        // No supervisor here, so an inherited socket from an outer supervised
        // session must not leak in — the inner session's hooks would relay to
        // the wrong server
        command.env_remove(crate::hook_protocol::SOCKET_ENV_VAR);
        let status = command
            .status()
            .with_context(|| format!("could not launch {cmd} — is it on your PATH?"))?;
        Ok(exit_code(&status))
    }
}

/// The settings overlay is a claude feature, so it's only for a claude launch.
/// A wrapper like `ax run --` ends in claude; only an explicit codex/pi/
/// opencode command is not claude.
fn wraps_claude(cmd: &str) -> bool {
    !cmd.split_whitespace()
        .any(|word| matches!(Tool::parse(word), Some(tool) if tool != Tool::Claude))
}

fn compose(claude_cmd: &str, claude_args: Vec<String>) -> Result<Command> {
    let mut words = claude_cmd.split_whitespace();
    let Some(program) = words.next() else {
        bail!("--claude-cmd is empty — give it a command like \"claude\" or \"ax run --\"");
    };

    let mut command = Command::new(program);
    command.args(words).args(claude_args);
    Ok(command)
}

/// A plain `claude` launch reveals its config dir up front; a wrapper like
/// `ax run` only reveals it through hook frames, so those launches use the
/// dir recorded last time under the same key. First-ever launches skip
/// materialization — the skills appear from the second session on.
fn materialize_skills(claude_cmd: &str, launch_key: &str) {
    let config_dir = if claude_cmd.split_whitespace().next() == Some("claude") {
        Some(paths::claude_config_home())
    } else {
        launches::config_dir_for(launch_key)
    };

    if let Some(config_dir) = config_dir {
        let outcome = Memory::open(&paths::memory_dir())
            .and_then(|memory| virtual_skills::materialize(&memory, &config_dir));
        if let Err(error) = outcome {
            eprintln!("note: could not materialize generated skills: {error:#}");
        }
    }
}

/// A `--settings` the user forwarded themselves gets folded into our overlay,
/// since claude's behavior for a repeated flag is undocumented.
fn extract_user_settings(claude_args: &[String]) -> Result<(Vec<String>, Option<PathBuf>)> {
    let mut remaining = Vec::new();
    let mut settings = None;
    let mut arguments = claude_args.iter();

    while let Some(argument) = arguments.next() {
        if argument == "--settings" {
            settings = Some(PathBuf::from(arguments.next().context(
                "--settings needs a file path — claude would refuse this too",
            )?));
        } else if let Some(value) = argument.strip_prefix("--settings=") {
            settings = Some(PathBuf::from(value));
        } else {
            remaining.push(argument.clone());
        }
    }
    Ok((remaining, settings))
}

pub fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_settings_are_extracted_in_both_flag_forms() {
        let (remaining, settings) = extract_user_settings(&[
            "--model".into(),
            "opus".into(),
            "--settings".into(),
            "/tmp/mine.json".into(),
        ])
        .unwrap();
        assert_eq!(remaining, vec!["--model", "opus"]);
        assert_eq!(settings, Some(PathBuf::from("/tmp/mine.json")));

        let (remaining, settings) =
            extract_user_settings(&["--settings=/tmp/mine.json".into()]).unwrap();
        assert!(remaining.is_empty());
        assert_eq!(settings, Some(PathBuf::from("/tmp/mine.json")));

        assert!(extract_user_settings(&["--settings".into()]).is_err());
    }
}
