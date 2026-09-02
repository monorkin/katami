//! Composing and launching the wrapped claude command.
//!
//! `--claude-cmd` names the launcher — a bare `claude`, or a wrapper like
//! `ax run --account work --` — and is split on whitespace: shell quoting
//! isn't interpreted, so anything fancier belongs in a wrapper script. The
//! forwarded arguments after `--` are appended verbatim.

use anyhow::{Context, Result, bail};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

use crate::launches;
use crate::memory::Memory;
use crate::overlay;
use crate::paths;
use crate::pty;
use crate::supervisor;
use crate::virtual_skills;

pub fn run(claude_cmd: &str, claude_args: &[String], exec: bool) -> Result<()> {
    if exec {
        let mut command = compose(claude_cmd, claude_args.to_vec())?;
        return Err(command.exec())
            .with_context(|| format!("could not launch {claude_cmd} — is it on your PATH?"));
    }

    let (mut claude_args, user_settings) = extract_user_settings(claude_args)?;
    let overlay_path = overlay::write(user_settings.as_deref())?;
    claude_args.push("--settings".into());
    claude_args.push(overlay_path.display().to_string());

    let launch_key = launches::key(&std::env::current_dir()?, claude_cmd);
    materialize_skills(claude_cmd, &launch_key);
    let mut command = compose(claude_cmd, claude_args)?;

    let code = if pty::is_terminal(libc::STDIN_FILENO) && pty::is_terminal(libc::STDOUT_FILENO) {
        supervisor::supervise(command, launch_key)
    } else {
        // No supervisor here, so an inherited socket from an outer supervised
        // session must not leak in — the inner session's hooks would relay to
        // the wrong server
        command.env_remove(supervisor::SOCKET_ENV_VAR);
        let status = command
            .status()
            .with_context(|| format!("could not launch {claude_cmd} — is it on your PATH?"))?;
        Ok(exit_code(&status))
    };

    overlay::remove(&overlay_path);
    std::process::exit(code?);
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

fn exit_code(status: &std::process::ExitStatus) -> i32 {
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
