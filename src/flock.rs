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
    let file = open(path)?;
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        Ok(Some(Flock { _file: file }))
    } else {
        Ok(None)
    }
}

/// Waits for the lock instead of skipping — for work that must not be lost,
/// like the final review of a session that just ended.
pub fn acquire(path: &Path, patience: std::time::Duration) -> Result<Option<Flock>> {
    let deadline = std::time::Instant::now() + patience;
    loop {
        if let Some(lock) = try_acquire(path)? {
            return Ok(Some(lock));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn open(path: &Path) -> Result<File> {
    let parent = path.parent().context("lock path has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    File::create(path).with_context(|| format!("could not create the lock file {}", path.display()))
}
