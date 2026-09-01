//! One-per-purpose process locks, flock style.
//!
//! The reviewer and curator each take one of these so concurrent sessions
//! don't double-run them. Non-blocking on purpose: whoever loses the race
//! just skips the work — the winner is already doing it. The lock lives as
//! long as the file handle; a crashed holder's lock dies with its process.

use anyhow::{Context, Result};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

pub struct Flock {
    _file: File,
}

pub fn try_acquire(path: &Path) -> Result<Option<Flock>> {
    let parent = path.parent().context("lock path has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let file = File::create(path)
        .with_context(|| format!("could not create the lock file {}", path.display()))?;

    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        Ok(Some(Flock { _file: file }))
    } else {
        Ok(None)
    }
}
