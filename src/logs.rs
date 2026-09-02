//! One appender for every component's activity log. The files under the logs
//! directory are what `agent log` merges, so every line gets the same shape:
//! an ISO timestamp, a space, the message. Logging never fails loudly — a
//! full disk shouldn't take a hook or a reviewer down with it.

use std::io::Write;

use crate::clock;
use crate::paths;

pub fn append(component: &str, message: &str) {
    let directory = paths::logs_dir();
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let path = directory.join(format!("{component}.log"));
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {message}", clock::timestamp());
    }
}
