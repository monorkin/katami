//! A bundle is how memories leave one store and arrive in another.
//!
//! It's a zip of markdown files, one per memory, with the fields that aren't
//! prose in a frontmatter block — so a bundle can be unzipped, read, corrected
//! by hand (a project path that differs on the other machine, say), zipped
//! back up, and imported. The frontmatter is flat `key: value` lines rather
//! than real YAML: every field is a scalar, the title lives in the heading
//! where it needs no quoting, and a hand-rolled parser can refuse anything it
//! doesn't recognize instead of guessing.
//!
//! Nothing is ever extracted to disk — entries are read straight out of the
//! zip into memory, by name, with a size cap — so a hostile bundle has no
//! path to climb out of and no room to balloon.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::clock::timestamp;
use crate::memory::{CLASSES, Kind, Portable};

const FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.json";
const MEMORIES_DIRECTORY: &str = "memories/";
const ENTRY_BYTES_LIMIT: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    katami: String,
    exported: String,
    memories: usize,
}

pub fn write(path: &Path, memories: &[(i64, Portable)]) -> Result<()> {
    let file = std::fs::File::create_new(path)
        .with_context(|| format!("could not create {} — pick another path with --to", path.display()))?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let manifest = Manifest {
        format: FORMAT,
        katami: env!("CARGO_PKG_VERSION").to_string(),
        exported: timestamp(),
        memories: memories.len(),
    };
    zip.start_file(MANIFEST, options)?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())?;

    for (id, portable) in memories {
        zip.start_file(format!("{MEMORIES_DIRECTORY}{id:05}-{}.md", slug(&portable.title)), options)?;
        zip.write_all(to_markdown(portable).as_bytes())?;
    }
    zip.finish()?;
    Ok(())
}

pub fn read(path: &Path) -> Result<Vec<Portable>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("could not open {} — is that the bundle's path?", path.display()))?;
    let mut zip = ZipArchive::new(file)
        .with_context(|| format!("{} is not a zip — bundles come from `katami memory export`", path.display()))?;

    let manifest: Manifest = serde_json::from_str(&entry_text(&mut zip, MANIFEST)?)
        .context("the bundle's manifest.json did not parse")?;
    if manifest.format != FORMAT {
        bail!(
            "this bundle is format {} and this katami reads format {FORMAT} — upgrade with `katami upgrade`",
            manifest.format
        );
    }

    let mut names: Vec<String> = zip
        .file_names()
        .filter(|it| it.starts_with(MEMORIES_DIRECTORY) && it.ends_with(".md"))
        .map(str::to_string)
        .collect();
    names.sort();

    names
        .iter()
        .map(|name| {
            from_markdown(&entry_text(&mut zip, name)?).with_context(|| format!("{name} in the bundle is malformed"))
        })
        .collect()
}

fn entry_text(zip: &mut ZipArchive<std::fs::File>, name: &str) -> Result<String> {
    let entry = zip
        .by_name(name)
        .with_context(|| format!("the bundle has no {name} — it didn't come from `katami memory export`"))?;
    let mut text = String::new();
    entry.take(ENTRY_BYTES_LIMIT + 1).read_to_string(&mut text)?;
    if text.len() as u64 > ENTRY_BYTES_LIMIT {
        bail!("{name} is over {ENTRY_BYTES_LIMIT} bytes — no memory is that long, refusing the bundle");
    }
    Ok(text)
}

pub fn to_markdown(portable: &Portable) -> String {
    let mut frontmatter = vec![format!("kind: {}", portable.kind)];
    if let Some(class) = &portable.class {
        frontmatter.push(format!("class: {class}"));
    }
    if let Some(entity) = &portable.entity {
        frontmatter.push(format!("entity: {}", single_line(entity)));
    }
    frontmatter.push(format!("pinned: {}", portable.pinned));
    frontmatter.push(format!("archived: {}", portable.archived));
    if let Some(reason) = &portable.archive_reason {
        frontmatter.push(format!("archive_reason: {}", single_line(reason)));
    }
    frontmatter.push(format!("created: {}", portable.created));
    frontmatter.push(format!("updated: {}", portable.updated));
    for link in &portable.links {
        frontmatter.push(format!("link: {}", single_line(link)));
    }

    format!(
        "---\n{}\n---\n# {}\n\n{}\n",
        frontmatter.join("\n"),
        single_line(&portable.title),
        portable.body.trim()
    )
}

pub fn from_markdown(text: &str) -> Result<Portable> {
    let (frontmatter, content) = text
        .strip_prefix("---\n")
        .and_then(|it| it.split_once("\n---\n"))
        .context("it must open with a `---` frontmatter block")?;

    let now = timestamp();
    let mut kind = None;
    let mut portable = Portable {
        kind: Kind::Observation,
        class: None,
        entity: None,
        title: String::new(),
        body: String::new(),
        links: Vec::new(),
        pinned: false,
        archived: false,
        archive_reason: None,
        created: now.clone(),
        updated: now,
    };

    for line in frontmatter.lines().filter(|it| !it.trim().is_empty()) {
        let (key, value) = line
            .split_once(':')
            .with_context(|| format!("frontmatter line `{line}` is not `key: value`"))?;
        let value = value.trim();
        match key.trim() {
            "kind" => {
                kind = Some(
                    Kind::parse(value)
                        .with_context(|| format!("unknown kind `{value}` — it's observation, card, or status"))?,
                )
            }
            "class" => {
                if !CLASSES.contains(&value) {
                    bail!("unknown class `{value}` — it's one of {}", CLASSES.join(", "));
                }
                portable.class = Some(value.to_string());
            }
            "entity" => portable.entity = Some(value.to_string()),
            "pinned" => portable.pinned = flag(key, value)?,
            "archived" => portable.archived = flag(key, value)?,
            "archive_reason" => portable.archive_reason = Some(value.to_string()),
            "created" => portable.created = value.to_string(),
            "updated" => portable.updated = value.to_string(),
            "link" => portable.links.push(value.to_string()),
            other => bail!("unknown frontmatter key `{other}`"),
        }
    }
    portable.kind = kind.context("the frontmatter has no `kind`")?;

    let content = content.trim_start_matches('\n');
    let (heading, body) = content.split_once('\n').unwrap_or((content, ""));
    portable.title = heading
        .strip_prefix("# ")
        .map(str::trim)
        .filter(|it| !it.is_empty())
        .context("the first line after the frontmatter must be `# Title`")?
        .to_string();
    portable.body = body.trim().to_string();
    if portable.body.is_empty() {
        bail!("it has a title but no body");
    }
    Ok(portable)
}

fn flag(key: &str, value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => bail!("`{key}` is `{other}` — it's true or false"),
    }
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn slug(title: &str) -> String {
    let words: Vec<String> = title
        .split(|it: char| !it.is_alphanumeric())
        .filter(|it| !it.is_empty())
        .map(str::to_lowercase)
        .collect();
    words.join("-").chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rebase_preference() -> Portable {
        Portable {
            kind: Kind::Observation,
            class: Some("preference".into()),
            entity: Some("project:/home/someone/Work/app".into()),
            title: "Prefers rebase over merge".into(),
            body: "Rebase feature branches.\n\n---\n\n# Not a title\nSee [[Commit message style]].".into(),
            links: vec!["Commit message style".into(), "Review workflow".into()],
            pinned: true,
            archived: true,
            archive_reason: Some("superseded".into()),
            created: "2026-09-02T12:26:00Z".into(),
            updated: "2026-09-08T06:46:00Z".into(),
        }
    }

    #[test]
    fn markdown_round_trips_every_field() {
        let portable = rebase_preference();
        assert_eq!(from_markdown(&to_markdown(&portable)).unwrap(), portable);

        let bare = Portable {
            class: None,
            entity: None,
            links: vec![],
            pinned: false,
            archived: false,
            archive_reason: None,
            ..rebase_preference()
        };
        assert_eq!(from_markdown(&to_markdown(&bare)).unwrap(), bare);
    }

    #[test]
    fn hand_written_memories_need_only_a_kind_a_title_and_a_body() {
        let portable = from_markdown("---\nkind: card\n---\n\n# Jason\n\nWorks on the iOS app.\n").unwrap();
        assert_eq!(portable.kind, Kind::Card);
        assert_eq!(portable.title, "Jason");
        assert_eq!(portable.body, "Works on the iOS app.");
        assert!(!portable.pinned && !portable.archived);
        assert!(!portable.created.is_empty());
    }

    #[test]
    fn malformed_memories_are_refused_with_the_reason() {
        let refusals = [
            ("# No frontmatter\n\nBody.", "frontmatter block"),
            ("---\nclass: preference\n---\n# T\n\nB", "no `kind`"),
            ("---\nkind: note\n---\n# T\n\nB", "unknown kind"),
            ("---\nkind: observation\nclass: vibe\n---\n# T\n\nB", "unknown class"),
            ("---\nkind: observation\npinned: yes\n---\n# T\n\nB", "true or false"),
            ("---\nkind: observation\ncolour: red\n---\n# T\n\nB", "unknown frontmatter key"),
            ("---\nkind: observation\n---\nNo heading\n\nB", "`# Title`"),
            ("---\nkind: observation\n---\n# Only a title\n", "no body"),
        ];
        for (text, reason) in refusals {
            let error = from_markdown(text).unwrap_err().to_string();
            assert!(error.contains(reason), "`{error}` should mention `{reason}`");
        }
    }

    #[test]
    fn bundles_round_trip_through_a_zip() {
        let directory = std::env::temp_dir().join(format!("katami-bundle-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("bundle.zip");
        let _ = std::fs::remove_file(&path);

        let card = Portable { kind: Kind::Card, title: "App".into(), ..rebase_preference() };
        write(&path, &[(7, rebase_preference()), (12, card.clone())]).unwrap();
        assert_eq!(read(&path).unwrap(), vec![rebase_preference(), card]);

        assert!(write(&path, &[]).is_err(), "an existing file must not be overwritten");

        std::fs::write(directory.join("not-a-bundle.zip"), "plain text").unwrap();
        assert!(read(&directory.join("not-a-bundle.zip")).is_err());

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn slugs_are_safe_file_names() {
        assert_eq!(slug("David's Set Aside at 500+ threads"), "david-s-set-aside-at-500-threads");
        assert_eq!(slug("/home/someone/Work/app"), "home-someone-work-app");
    }
}
