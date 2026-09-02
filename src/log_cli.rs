//! `agent log`: what the supervisor, reviewer, and curator have been doing.
//!
//! Each of them appends timestamped lines to its own file under the logs
//! directory; this merges them into one chronological stream so nobody has
//! to know which file to tail. Timestamps are ISO strings, so sorting the
//! lines sorts the events.

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::paths;

struct LogEntry {
    timestamp: String,
    source: String,
    message: String,
}

impl LogEntry {
    fn print(&self) {
        println!("{}  {:<10}  {}", self.timestamp, self.source, self.message);
    }
}

pub fn print(lines: usize, follow: bool) -> Result<()> {
    let mut offsets: BTreeMap<PathBuf, u64> = BTreeMap::new();
    let mut entries = collect(&mut offsets)?;
    entries.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    if entries.is_empty() && !follow {
        println!("No activity yet — logs appear once a supervised session runs.");
        return Ok(());
    }
    for entry in entries.iter().rev().take(lines).rev() {
        entry.print();
    }

    while follow {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let mut fresh = collect(&mut offsets)?;
        fresh.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        for entry in &fresh {
            entry.print();
        }
    }
    Ok(())
}

/// Reads every log file past its last-seen offset, so the first call returns
/// everything and later calls (in follow mode) return only what's new.
fn collect(offsets: &mut BTreeMap<PathBuf, u64>) -> Result<Vec<LogEntry>> {
    let mut entries = Vec::new();
    let Ok(files) = std::fs::read_dir(paths::logs_dir()) else {
        return Ok(entries);
    };

    for file in files.flatten() {
        let path = file.path();
        let source = source_of(&path);
        let offset = offsets.entry(path.clone()).or_insert(0);

        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        if *offset as usize > contents.len() {
            *offset = 0;
        }
        let unseen = &contents[*offset as usize..];
        let consumed: usize = unseen
            .split_inclusive('\n')
            .filter(|it| it.ends_with('\n'))
            .map(|line| {
                if let Some((timestamp, message)) = line.trim_end().split_once(' ') {
                    entries.push(LogEntry {
                        timestamp: timestamp.to_string(),
                        source: source.clone(),
                        message: message.to_string(),
                    });
                }
                line.len()
            })
            .sum();
        *offset += consumed as u64;
    }
    Ok(entries)
}

fn source_of(path: &std::path::Path) -> String {
    let name = path.file_stem().unwrap_or_default().to_string_lossy();
    name.split('-').next().unwrap_or("unknown").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_come_from_file_names() {
        assert_eq!(source_of(std::path::Path::new("/x/supervisor-123.log")), "supervisor");
        assert_eq!(source_of(std::path::Path::new("/x/reviewer.log")), "reviewer");
        assert_eq!(source_of(std::path::Path::new("/x/curator.log")), "curator");
    }
}
