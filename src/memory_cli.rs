//! `agent memory`: the human's window into the store — and a debugging
//! surface for everything the hooks do silently.

use anyhow::Result;

use crate::cards;
use crate::embeddings;
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

fn open() -> Result<Memory> {
    Memory::open(&paths::memory_dir())
}
