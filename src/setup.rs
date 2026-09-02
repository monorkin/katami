//! `katami setup`: the one post-install step.
//!
//! Installs shell completion for the current shell and downloads the
//! embedding model that powers semantic search. It's the stable entry point
//! the install instructions point at, so if first-run setup grows another
//! step later, people don't have to relearn anything.

use anyhow::Result;
use std::path::Path;

use crate::completions;
use crate::embeddings;

pub fn run() -> Result<()> {
    match current_shell() {
        Some(shell) => completions::install(&shell)?,
        None => eprintln!(
            "couldn't tell which shell you use — install completion yourself with `katami shell-completion install <shell>`"
        ),
    }

    if embeddings::available() {
        println!("Semantic search model already installed.");
    } else {
        embeddings::pull()?;
    }
    Ok(())
}

fn current_shell() -> Option<String> {
    let shell = std::env::var("SHELL").ok()?;
    Path::new(&shell)
        .file_name()?
        .to_str()
        .map(str::to_string)
}
