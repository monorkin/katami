//! Cards: the distilled current state of one entity, rendered as markdown.
//!
//! Observations are what sessions produce; a card is what the curator folds
//! them into — one per person or project, with stable sections. The database
//! row is the source of truth and the markdown file under `memory/cards/` is
//! a rendered view for the human, refreshed after every card mutation.

use anyhow::Result;
use std::path::Path;

use crate::fsutil;
use crate::memory::{Memory, Stored};

pub fn extract_links(body: &str) -> Vec<String> {
    let mut links = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        rest = &rest[start + 2..];
        if let Some(end) = rest.find("]]") {
            let link = rest[..end].trim();
            if !link.is_empty() && !links.iter().any(|it| it == link) {
                links.push(link.to_string());
            }
            rest = &rest[end + 2..];
        } else {
            break;
        }
    }
    links
}

/// The directory is a view of the store and nothing else, so a markdown file
/// there that no current card renders to is a leftover — a card that was
/// retitled, archived, or re-identified — and goes.
pub fn render_all(memory: &Memory, cards_dir: &Path) -> Result<()> {
    let mut rendered = Vec::new();
    for card in memory.list()?.iter().filter(|it| it.kind == crate::memory::Kind::Card) {
        render(card, cards_dir)?;
        rendered.push(file_name(card));
    }

    if cards_dir.is_dir() {
        for entry in std::fs::read_dir(cards_dir)? {
            let path = entry?.path();
            let name = path.file_name().and_then(|it| it.to_str()).unwrap_or("").to_string();
            if name.ends_with(".md") && !rendered.contains(&name) {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

pub fn render(card: &Stored, cards_dir: &Path) -> Result<()> {
    let path = cards_dir.join(file_name(card));
    let mut contents = format!("# {}\n", card.title);
    if let Some(entity) = &card.entity {
        contents.push_str(&format!("\n_{entity}_\n"));
    }
    contents.push_str(&format!("\n{}\n", card.body.trim_end()));
    fsutil::write_atomically(&path, &contents)
}

/// The id suffix keeps two cards whose titles slug identically from silently
/// overwriting each other's rendered file.
fn file_name(card: &Stored) -> String {
    format!("{}-{}.md", slug(&card.title), card.id)
}

pub fn slug(title: &str) -> String {
    let mut slug = String::new();
    for character in title.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_string()
}

/// The human half of an entity string: `project:/x/app` → `/x/app`,
/// `person:Jason` → `Jason`.
pub fn entity_name(entity: &str) -> &str {
    entity.split_once(':').map_or(entity, |it| it.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_are_extracted_once_each_and_trimmed() {
        let body = "See [[ax uses flocks]] and [[ Cards ]] — also [[ax uses flocks]] again, [[";
        assert_eq!(extract_links(body), vec!["ax uses flocks", "Cards"]);
    }

    #[test]
    fn rendering_every_card_sweeps_files_no_card_renders_to() {
        use crate::memory::{Kind, NewMemory};

        let cards_dir = std::env::temp_dir().join(format!("katami-cards-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cards_dir);
        std::fs::create_dir_all(&cards_dir).unwrap();
        std::fs::write(cards_dir.join("jason-12.md"), "# Jason\n").unwrap();
        std::fs::write(cards_dir.join("notes.txt"), "not a card").unwrap();

        let memory = Memory::open_in_memory().unwrap();
        let id = memory
            .add(&NewMemory {
                kind: Kind::Card,
                entity: Some("person:jason".into()),
                title: "Jason".into(),
                body: "Works on the iOS app.".into(),
                links: vec![],
                source_session: None,
                class: None,
            })
            .unwrap();
        render_all(&memory, &cards_dir).unwrap();

        let mut files: Vec<String> = std::fs::read_dir(&cards_dir)
            .unwrap()
            .map(|it| it.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        files.sort();
        assert_eq!(files, vec![format!("jason-{id}.md"), "notes.txt".to_string()]);

        std::fs::remove_dir_all(&cards_dir).unwrap();
    }

    #[test]
    fn slugs_flatten_punctuation_and_case() {
        assert_eq!(slug("Stanko's ax — profile #2"), "stanko-s-ax-profile-2");
    }

    #[test]
    fn entity_names_drop_the_kind_prefix() {
        assert_eq!(entity_name("project:/home/x/app"), "/home/x/app");
        assert_eq!(entity_name("person:Jason"), "Jason");
        assert_eq!(entity_name("bare"), "bare");
    }
}
