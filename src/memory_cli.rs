//! `katami memory`: the human's window into the store — and a debugging
//! surface for everything the hooks do silently.

use anyhow::{Context, Result};

use crate::cards;
use crate::embeddings;
use crate::fsutil;
use crate::memory::{Kind, ListFilter, Listing, Memory, NewMemory, SortColumn, SortKey};
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
    let id = memory.add(&NewMemory {
        kind,
        entity,
        title: title.to_string(),
        body: body.to_string(),
        links: all_links,
        source_session: None,
        class: None,
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
        println!("Keyword search only — run `katami memory pull-models` to enable semantic search.");
    }

    let hits = search::hybrid(&memory, query, 10)?;
    if hits.is_empty() {
        println!("No memories match — see `katami memory list` for what's stored.");
        return Ok(());
    }

    println!("{:>4}  {:<10}  {:<11}  title", "id", "updated", "kind");
    for hit in hits {
        let stored = memory.get(hit.id)?;
        println!(
            "{:>4}  {:<10}  {:<11}  {}",
            stored.id,
            &stored.updated[..10],
            stored.kind,
            stored.title
        );
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

pub fn list(filter: ListFilter, kinds: Option<&str>, sort_by: Option<&str>) -> Result<()> {
    let listing = Listing {
        filter,
        kinds: parse_kinds(kinds.unwrap_or(""))?,
        order: parse_sort(sort_by.unwrap_or(""))?,
    };
    let memory = open()?;
    let rows = memory.overview(&listing)?;
    if rows.is_empty() {
        println!("No memories here — sessions will add them, or use `katami memory add`.");
        return Ok(());
    }

    println!(
        "{:>4}  {:<10}  {:<11}  {:>5}  {:<10}  title",
        "id", "updated", "kind", "uses", "last used"
    );
    for row in rows {
        let mut title = row.stored.title.clone();
        if row.stored.pinned {
            title.push_str(" [pinned]");
        }
        if row.stored.archived {
            title.push_str(" [archived]");
        }
        let last_used = match &row.last_used {
            Some(timestamp) => &timestamp[..10],
            None => "never",
        };
        println!(
            "{:>4}  {:<10}  {:<11}  {:>5}  {last_used:<10}  {title}",
            row.stored.id,
            &row.stored.updated[..10],
            row.stored.kind,
            row.uses
        );
    }
    Ok(())
}

fn parse_kinds(text: &str) -> Result<Vec<Kind>> {
    text.split(',')
        .map(str::trim)
        .filter(|it| !it.is_empty())
        .map(|name| {
            Kind::parse(name)
                .with_context(|| format!("unknown kind '{name}' — expected observation, card, or status"))
        })
        .collect()
}

/// Accepts the SQL-looking form people reach for — `last_used desc, uses` —
/// but only ever yields typed keys; nothing here reaches the query as text.
fn parse_sort(text: &str) -> Result<Vec<SortKey>> {
    text.split(',')
        .map(str::trim)
        .filter(|it| !it.is_empty())
        .map(parse_sort_key)
        .collect()
}

fn parse_sort_key(term: &str) -> Result<SortKey> {
    let mut words = term.split_whitespace();
    let column = words.next().expect("terms are non-empty");
    let direction = words.next().map(str::to_ascii_lowercase);
    if words.next().is_some() {
        anyhow::bail!("could not read sort term '{term}' — expected a column, optionally followed by asc or desc");
    }

    let column = match column.to_ascii_lowercase().as_str() {
        "id" => SortColumn::Id,
        "updated" => SortColumn::Updated,
        "kind" => SortColumn::Kind,
        "uses" => SortColumn::Uses,
        "last_used" | "last-used" => SortColumn::LastUsed,
        "title" => SortColumn::Title,
        other => anyhow::bail!(
            "unknown sort column '{other}' — expected id, updated, kind, uses, last_used, or title"
        ),
    };
    let descending = match direction.as_deref() {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(other) => anyhow::bail!("unknown sort direction '{other}' — expected asc or desc"),
    };
    Ok(SortKey { column, descending })
}

pub fn archive(id: i64) -> Result<()> {
    let memory = open()?;
    let stored = memory.get(id)?;
    memory.archive(id, "manual")?;
    println!(
        "Archived {id}: {} — bring it back with `katami memory unarchive {id}`",
        stored.title
    );
    Ok(())
}

pub fn unarchive(id: i64) -> Result<()> {
    let memory = open()?;
    memory.unarchive(id)?;
    println!("Unarchived {id}: {}", memory.get(id)?.title);
    Ok(())
}

pub fn edit(id: i64) -> Result<()> {
    let memory = open()?;
    let stored = memory.get(id)?;

    let path = std::env::temp_dir().join(format!("katami-memory-{id}.md"));
    fsutil::write_atomically(&path, &edit_buffer(&stored))?;
    launch_editor(&path)?;
    let edited = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);

    let (title, entity, body) = parse_edit_buffer(&edited)?;
    memory.update(id, &title, &body, entity.as_deref())?;
    memory.replace_links(id, &cards::extract_links(&body))?;
    embeddings::embed_into(&memory, id, &format!("{title}\n{body}"))?;
    if stored.kind == Kind::Card {
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

    #[test]
    fn kinds_are_parsed_from_a_comma_list() {
        assert_eq!(parse_kinds("card, status").unwrap(), vec![Kind::Card, Kind::Status]);
        assert!(parse_kinds("").unwrap().is_empty());
        assert!(parse_kinds("skill").is_err());
    }

    #[test]
    fn sort_terms_become_typed_keys() {
        assert_eq!(
            parse_sort("last_used DESC, uses desc, title").unwrap(),
            vec![
                SortKey { column: SortColumn::LastUsed, descending: true },
                SortKey { column: SortColumn::Uses, descending: true },
                SortKey { column: SortColumn::Title, descending: false },
            ]
        );
        assert!(parse_sort("").unwrap().is_empty());
        assert!(parse_sort("uses; DROP TABLE memories").is_err());
        assert!(parse_sort("uses sideways").is_err());
        assert!(parse_sort("body desc").is_err());
    }
}
