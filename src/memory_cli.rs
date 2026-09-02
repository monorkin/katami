//! `agent memory`: the human's window into the store — and a debugging
//! surface for everything the hooks do silently.

use anyhow::{Context, Result};

use crate::cards;
use crate::embeddings;
use crate::fsutil;
use crate::memory::{Kind, Memory, NewObservation};
use crate::paths;
use crate::search;

pub fn add(
    title: &str,
    body: &str,
    entity: Option<String>,
    links: Vec<String>,
    card: bool,
) -> Result<()> {
    let memory = open()?;
    let mut all_links = cards::extract_links(body);
    for link in links {
        if !all_links.contains(&link) {
            all_links.push(link);
        }
    }

    let kind = if card { Kind::Card } else { Kind::Observation };
    let id = memory.add(&NewObservation {
        kind,
        entity,
        title: title.to_string(),
        body: body.to_string(),
        links: all_links,
        source_session: None,
    })?;
    embeddings::embed_into(&memory, id, &format!("{title}\n{body}"))?;

    if card {
        cards::render(&memory.get(id)?, &paths::memory_dir().join("cards"))?;
    }
    println!("Added memory {id}: {title}");
    Ok(())
}

pub fn search(query: &str) -> Result<()> {
    let memory = open()?;
    if !embeddings::available() {
        println!("Keyword search only — run `agent memory pull-models` to enable semantic search.");
    }

    let hits = search::hybrid(&memory, query, 10)?;
    if hits.is_empty() {
        println!("No memories match — see `agent memory list` for what's stored.");
        return Ok(());
    }

    for hit in hits {
        let stored = memory.get(hit.id)?;
        println!("{:>4}  {}  {}", stored.id, stored.updated, stored.title);
    }
    Ok(())
}

pub fn show(id: i64) -> Result<()> {
    let memory = open()?;
    let stored = memory.get(id)?;

    println!("# {} ({})", stored.title, stored.kind);
    if let Some(entity) = &stored.entity {
        println!("entity: {entity}");
    }
    if stored.archived {
        println!("archived: yes");
    }
    println!("updated: {}\n\n{}", stored.updated, stored.body);

    let neighbors = memory.neighbors(id)?;
    if !neighbors.is_empty() {
        println!();
        for neighbor in neighbors {
            println!("linked: [[{}]] (id {})", neighbor.title, neighbor.id);
        }
    }
    Ok(())
}

pub fn list() -> Result<()> {
    let memory = open()?;
    let all = memory.list()?;
    if all.is_empty() {
        println!("No memories yet — sessions will add them, or use `agent memory add`.");
        return Ok(());
    }

    for stored in all {
        let mut flags = String::new();
        if stored.kind == "card" {
            flags.push_str(" [card]");
        }
        if stored.pinned {
            flags.push_str(" [pinned]");
        }
        println!("{:>4}  {}  {}{flags}", stored.id, stored.updated, stored.title);
    }
    Ok(())
}

pub fn archive(id: i64) -> Result<()> {
    let memory = open()?;
    let stored = memory.get(id)?;
    memory.archive(id)?;
    println!("Archived {id}: {} — restore with `agent memory restore {id}`", stored.title);
    Ok(())
}

pub fn restore(id: i64) -> Result<()> {
    let memory = open()?;
    memory.restore(id)?;
    println!("Restored {id}: {}", memory.get(id)?.title);
    Ok(())
}

pub fn edit(id: i64) -> Result<()> {
    let memory = open()?;
    let stored = memory.get(id)?;

    let path = std::env::temp_dir().join(format!("agent-memory-{id}.md"));
    fsutil::write_atomically(&path, &edit_buffer(&stored))?;
    launch_editor(&path)?;
    let edited = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);

    let (title, entity, body) = parse_edit_buffer(&edited)?;
    memory.update(id, &title, &body, entity.as_deref())?;
    memory.replace_links(id, &cards::extract_links(&body))?;
    embeddings::embed_into(&memory, id, &format!("{title}\n{body}"))?;
    if stored.kind == "card" {
        cards::render(&memory.get(id)?, &paths::memory_dir().join("cards"))?;
    }
    println!("Updated {id}: {title}");
    Ok(())
}

fn edit_buffer(stored: &crate::memory::Stored) -> String {
    let entity = stored.entity.as_deref().unwrap_or("");
    format!("# {}\nentity: {entity}\n\n{}\n", stored.title, stored.body.trim_end())
}

fn parse_edit_buffer(contents: &str) -> Result<(String, Option<String>, String)> {
    let mut lines = contents.lines();
    let title = lines
        .next()
        .and_then(|it| it.strip_prefix("# "))
        .map(str::trim)
        .filter(|it| !it.is_empty())
        .context("the first line must be `# Title`")?
        .to_string();

    let mut entity = None;
    let mut body_lines: Vec<&str> = Vec::new();
    for line in lines {
        if body_lines.is_empty() && entity.is_none()
            && let Some(value) = line.strip_prefix("entity:")
        {
            let value = value.trim();
            entity = Some(if value.is_empty() { String::new() } else { value.to_string() });
            continue;
        }
        if body_lines.is_empty() && line.trim().is_empty() {
            continue;
        }
        body_lines.push(line);
    }

    let body = body_lines.join("\n").trim_end().to_string();
    if body.is_empty() {
        anyhow::bail!("the body is empty — archive the memory instead of blanking it");
    }
    Ok((title, entity.filter(|it| !it.is_empty()), body))
}

fn launch_editor(path: &std::path::Path) -> Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());

    let status = std::process::Command::new(&editor)
        .arg(path)
        .status()
        .with_context(|| format!("could not launch {editor} — set $EDITOR"))?;
    if !status.success() {
        anyhow::bail!("{editor} exited with {status}; the memory was not changed");
    }
    Ok(())
}

pub fn stats() -> Result<()> {
    let memory = open()?;
    let stats = memory.usage_stats()?;
    if stats.is_empty() {
        println!("Nothing used yet — stats appear once sessions inject memories or invoke skills.");
        return Ok(());
    }

    println!("{:>5}  {:<20}  {:<8}  name", "uses", "last used", "kind");
    for stat in stats {
        println!(
            "{:>5}  {:<20}  {:<8}  {}",
            stat.uses, stat.last_used, stat.kind, stat.name
        );
    }
    Ok(())
}

fn open() -> Result<Memory> {
    Memory::open(&paths::memory_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_buffers_round_trip() {
        let buffer = "# New title\nentity: person:Kevin\n\nThe body.\nSecond line.\n";
        let (title, entity, body) = parse_edit_buffer(buffer).unwrap();
        assert_eq!(title, "New title");
        assert_eq!(entity.as_deref(), Some("person:Kevin"));
        assert_eq!(body, "The body.\nSecond line.");

        let (_, entity, _) = parse_edit_buffer("# T\nentity:\n\nBody.\n").unwrap();
        assert!(entity.is_none());

        assert!(parse_edit_buffer("no heading\n\nBody.\n").is_err());
        assert!(parse_edit_buffer("# T\nentity: x\n\n\n").is_err());
    }
}
