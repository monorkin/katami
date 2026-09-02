//! Composing and launching the wrapped coding tool.
//!
//! The command is whatever follows `katami` — a bare `claude`, `codex`, `pi`,
//! `opencode`, or a wrapper like `ax --account work -- --dangerously-skip-permissions`.
//! The first word is the program; the rest are its arguments, passed verbatim.
//!
//! Every launch first installs the relays into codex, pi, and opencode, so a
//! session that shells out to another tool mid-run relays to this same
//! supervisor. Claude alone needs a per-launch settings overlay and skill
//! materialization; the others carry their integration in their relays.

use anyhow::{Context, Result, bail};
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

pub fn run(command: &[String]) -> Result<()> {
    let _ = relays::install_all();

    let launch_key = launches::key(&std::env::current_dir()?, &command.join(" "));
    let mut args: Vec<String> = command[1..].to_vec();
    let mut overlay_path = None;

    if wraps_claude(command) {
        let (rest, user_settings) = extract_user_settings(&args)?;
        let path = overlay::write(user_settings.as_deref())?;
        // Appended at the very end so a wrapper like `ax … -- …` lands it past
        // the final `--`, where claude's own args live.
        args = rest;
        args.push("--settings".into());
        args.push(path.display().to_string());
        materialize_skills(command, &launch_key);
        overlay_path = Some(path);
    }

    let composed = compose(&command[0], &args)?;
    let code = supervise_or_pipe(composed, &command[0], launch_key);

    if let Some(path) = &overlay_path {
        overlay::remove(path);
    }
    std::process::exit(code?);
}

fn supervise_or_pipe(mut command: Command, program: &str, launch_key: String) -> Result<i32> {
    if pty::is_terminal(libc::STDIN_FILENO) && pty::is_terminal(libc::STDOUT_FILENO) {
        supervisor::supervise(command, launch_key)
    } else {
        // No supervisor here, so an inherited socket from an outer supervised
        // session must not leak in — the inner session's hooks would relay to
        // the wrong server
        command.env_remove(crate::hook_protocol::SOCKET_ENV_VAR);
        let status = command
            .status()
            .with_context(|| format!("could not launch {program} — is it on your PATH?"))?;
        Ok(exit_code(&status))
    }
}

/// The settings overlay is a claude feature, so it's only for a claude launch.
/// A wrapper like `ax … --` ends in claude; only an explicit codex/pi/opencode
/// command is not claude.
fn wraps_claude(command: &[String]) -> bool {
    !command
        .iter()
        .any(|word| matches!(Tool::parse(word), Some(tool) if tool != Tool::Claude))
}

fn compose(program: &str, args: &[String]) -> Result<Command> {
    if program.is_empty() {
        bail!("no command given — try `katami claude` or `katami codex`");
    }
    let mut command = Command::new(program);
    command.args(args);
    Ok(command)
}

/// A plain `claude` launch reveals its config dir up front; a wrapper like
/// `ax …` only reveals it through hook frames, so those launches use the dir
/// recorded last time under the same key. First-ever launches skip
/// materialization — the skills appear from the second session on.
fn materialize_skills(command: &[String], launch_key: &str) {
    let config_dir = if command.first().map(String::as_str) == Some("claude") {
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

/// A `--settings` the user passed themselves gets folded into our overlay,
/// since claude's behavior for a repeated flag is undocumented.
fn extract_user_settings(args: &[String]) -> Result<(Vec<String>, Option<PathBuf>)> {
    let mut remaining = Vec::new();
    let mut settings = None;
    let mut arguments = args.iter();

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

    #[test]
    fn wraps_claude_detects_the_launcher() {
        assert!(wraps_claude(&["claude".into()]));
        assert!(wraps_claude(&["ax".into(), "--account".into(), "work".into(), "--".into()]));
        assert!(!wraps_claude(&["codex".into()]));
        assert!(!wraps_claude(&["pi".into(), "--mode".into(), "json".into()]));
    }
}
