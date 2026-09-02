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

pub fn render_all(memory: &Memory, cards_dir: &Path) -> Result<()> {
    for card in memory.list()?.iter().filter(|it| it.kind == crate::memory::Kind::Card) {
        render(card, cards_dir)?;
    }
    Ok(())
}

pub fn render(card: &Stored, cards_dir: &Path) -> Result<()> {
    // The id suffix keeps two cards whose titles slug identically from
    // silently overwriting each other's rendered file
    let path = cards_dir.join(format!("{}-{}.md", slug(&card.title), card.id));
    let mut contents = format!("# {}\n", card.title);
    if let Some(entity) = &card.entity {
        contents.push_str(&format!("\n_{entity}_\n"));
    }
    contents.push_str(&format!("\n{}\n", card.body.trim_end()));
    fsutil::write_atomically(&path, &contents)
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

pub fn project_entity(cwd: &Path) -> String {
    format!("project:{}", cwd.display())
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
