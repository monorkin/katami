//! Shell completion scripts: printing one, and putting one where its shell looks for it.

use anyhow::{Context, Result, bail};
use usage::complete::Shell;
use usage::install::{Env, Installed, Loading, OnForeign, Wrote};

use crate::Cli;

pub fn print(shell: &str) -> Result<()> {
    print!("{}", Cli::completion_script(named_shell(shell)?));
    Ok(())
}

pub fn install(shell: &str) -> Result<()> {
    let shell = named_shell(shell)?;
    let installed = Cli::install_completion(shell, &Env::from_process(), OnForeign::Refuse)
        .with_context(|| format!("could not install the {} completions", shell.as_str()))?;
    report(&installed);
    Ok(())
}

fn named_shell(name: &str) -> Result<Shell> {
    match Shell::from_name(name) {
        Some(shell) => Ok(shell),
        None => bail!("unknown shell '{name}' — try bash, elvish, zsh, fish, nu, or powershell"),
    }
}

fn report(installed: &Installed) {
    let path = installed.plan.path.display();
    let shell = installed.plan.shell.as_str();
    match installed.wrote {
        Wrote::Unchanged => println!("{shell} completions were already current at {path}."),
        Wrote::Created => println!("Wrote {shell} completions to {path}."),
        _ => println!("Updated {shell} completions at {path}."),
    }

    if let Loading::Manual { line, file, .. } = &installed.plan.loading {
        println!("Add this to {file}:");
        println!("{line}");
    }
    if let Some(note) = installed.plan.note {
        println!("Note: {note}");
    }
}
