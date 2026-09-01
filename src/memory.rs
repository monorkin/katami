//! The memory store: everything agent learns, in one SQLite database.
//!
//! There is a single shared store — memories belong to the person, not to
//! whichever account or session produced them. The database is the source of
//! truth; the markdown cards under `memory/cards/` are rendered views of it.
//! WAL mode plus a busy timeout lets concurrent sessions write without
//! coordination; the reviewer and curator add their own flocks on top so
//! only one of each runs at a time.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

use crate::timestamp;

pub struct Memory {
    pub connection: Connection,
}

pub struct NewObservation {
    pub kind: Kind,
    pub entity: Option<String>,
    pub title: String,
    pub body: String,
    pub links: Vec<String>,
    pub source_session: Option<String>,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Kind {
    Observation,
    Card,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Observation => "observation",
            Kind::Card => "card",
        }
    }
}

pub struct GeneratedSkill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub created: String,
}

pub struct Stored {
    pub id: i64,
    pub kind: String,
    pub entity: Option<String>,
    pub title: String,
    pub body: String,
    pub pinned: bool,
    pub archived: bool,
    pub updated: String,
}

impl Memory {
    pub fn open(directory: &Path) -> Result<Memory> {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        let path = directory.join("store.db");
        let connection = Connection::open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.busy_timeout(std::time::Duration::from_millis(2000))?;

        let memory = Memory { connection };
        memory.migrate()?;
        Ok(memory)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Memory> {
        let memory = Memory {
            connection: Connection::open_in_memory()?,
        };
        memory.migrate()?;
        Ok(memory)
    }

    fn migrate(&self) -> Result<()> {
        let version: i64 = self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version >= 1 {
            return Ok(());
        }

        self.connection.execute_batch(
            "
            CREATE TABLE memories (
                id INTEGER PRIMARY KEY,
                kind TEXT NOT NULL,
                entity TEXT,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                created TEXT NOT NULL,
                updated TEXT NOT NULL,
                source_session TEXT,
                pinned INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0
            );

            CREATE VIRTUAL TABLE memories_fts USING fts5(
                title, body, entity,
                content='memories', content_rowid='id'
            );

            CREATE TRIGGER memories_insert AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, title, body, entity)
                VALUES (new.id, new.title, new.body, new.entity);
            END;
            CREATE TRIGGER memories_delete AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, title, body, entity)
                VALUES ('delete', old.id, old.title, old.body, old.entity);
            END;
            CREATE TRIGGER memories_update AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, title, body, entity)
                VALUES ('delete', old.id, old.title, old.body, old.entity);
                INSERT INTO memories_fts(rowid, title, body, entity)
                VALUES (new.id, new.title, new.body, new.entity);
            END;

            CREATE TABLE links (
                from_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                to_title TEXT NOT NULL,
                PRIMARY KEY (from_id, to_title)
            );

            CREATE TABLE usage (
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                session_id TEXT NOT NULL,
                used_at TEXT NOT NULL
            );
            CREATE INDEX usage_by_name ON usage(kind, name, used_at);

            CREATE TABLE cursors (
                transcript_path TEXT PRIMARY KEY,
                byte_offset INTEGER NOT NULL,
                updated TEXT NOT NULL
            );

            CREATE TABLE curator_state (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE generated_skills (
                name TEXT PRIMARY KEY,
                description TEXT NOT NULL,
                instructions TEXT NOT NULL,
                created TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE embeddings (
                memory_id INTEGER PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
                model TEXT NOT NULL,
                vector BLOB NOT NULL
            );

            PRAGMA user_version = 1;
            ",
        )?;
        Ok(())
    }

    pub fn add(&self, observation: &NewObservation) -> Result<i64> {
        let now = timestamp();
        self.connection.execute(
            "INSERT INTO memories (kind, entity, title, body, created, updated, source_session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            rusqlite::params![
                observation.kind.as_str(),
                observation.entity,
                observation.title,
                observation.body,
                now,
                observation.source_session,
            ],
        )?;
        let id = self.connection.last_insert_rowid();

        for title in &observation.links {
            self.connection.execute(
                "INSERT OR IGNORE INTO links (from_id, to_title) VALUES (?1, ?2)",
                rusqlite::params![id, title],
            )?;
        }
        Ok(id)
    }

    pub fn get(&self, id: i64) -> Result<Stored> {
        self.connection
            .query_row(
                "SELECT id, kind, entity, title, body, pinned, archived, updated
                 FROM memories WHERE id = ?1",
                [id],
                row_to_stored,
            )
            .with_context(|| format!("no memory with id {id} — see `agent memory list`"))
    }

    pub fn list(&self) -> Result<Vec<Stored>> {
        let mut statement = self.connection.prepare(
            "SELECT id, kind, entity, title, body, pinned, archived, updated
             FROM memories WHERE archived = 0 ORDER BY updated DESC",
        )?;
        let rows = statement.query_map([], row_to_stored)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Everything one hop away over [[links]], in both directions: what this
    /// memory links to, and what links back to its title.
    pub fn neighbors(&self, id: i64) -> Result<Vec<Stored>> {
        let mut statement = self.connection.prepare(
            "SELECT m.id, m.kind, m.entity, m.title, m.body, m.pinned, m.archived, m.updated
             FROM memories m
             WHERE m.archived = 0 AND m.id != ?1 AND (
                m.title IN (SELECT to_title FROM links WHERE from_id = ?1)
                OR m.id IN (
                    SELECT from_id FROM links
                    WHERE to_title = (SELECT title FROM memories WHERE id = ?1)
                )
             )",
        )?;
        let rows = statement.query_map([id], row_to_stored)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_embedding(&self, id: i64, model: &str, vector: &[f32]) -> Result<()> {
        let mut blob = Vec::with_capacity(vector.len() * 4);
        for value in vector {
            blob.extend_from_slice(&value.to_le_bytes());
        }
        self.connection.execute(
            "INSERT INTO embeddings (memory_id, model, vector) VALUES (?1, ?2, ?3)
             ON CONFLICT (memory_id) DO UPDATE SET model = ?2, vector = ?3",
            rusqlite::params![id, model, blob],
        )?;
        Ok(())
    }

    pub fn embeddings(&self, model: &str) -> Result<Vec<(i64, Vec<f32>)>> {
        let mut statement = self.connection.prepare(
            "SELECT e.memory_id, e.vector FROM embeddings e
             JOIN memories m ON m.id = e.memory_id
             WHERE e.model = ?1 AND m.archived = 0",
        )?;
        let rows = statement.query_map([model], |row| {
            let id: i64 = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            let vector = blob
                .chunks_exact(4)
                .map(|it| f32::from_le_bytes([it[0], it[1], it[2], it[3]]))
                .collect();
            Ok((id, vector))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn record_usage(&self, kind: &str, name: &str, session_id: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO usage (kind, name, session_id, used_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![kind, name, session_id, timestamp()],
        )?;
        Ok(())
    }

    pub fn observations_for_entity(&self, entity: &str) -> Result<Vec<Stored>> {
        let mut statement = self.connection.prepare(
            "SELECT id, kind, entity, title, body, pinned, archived, updated
             FROM memories
             WHERE archived = 0 AND kind = 'observation' AND entity = ?1
             ORDER BY created ASC",
        )?;
        let rows = statement.query_map([entity], row_to_stored)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn card_for_entity(&self, entity: &str) -> Result<Option<Stored>> {
        let card = self
            .connection
            .query_row(
                "SELECT id, kind, entity, title, body, pinned, archived, updated
                 FROM memories
                 WHERE archived = 0 AND kind = 'card' AND entity = ?1",
                [entity],
                row_to_stored,
            )
            .ok();
        Ok(card)
    }

    pub fn entities_with_observations(&self, minimum: usize) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT entity FROM memories
             WHERE archived = 0 AND kind = 'observation' AND entity IS NOT NULL
             GROUP BY entity HAVING COUNT(*) >= ?1",
        )?;
        let rows = statement.query_map([minimum as i64], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update_body(&self, id: i64, body: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET body = ?2, updated = ?3 WHERE id = ?1",
            rusqlite::params![id, body, timestamp()],
        )?;
        Ok(())
    }

    pub fn archive(&self, id: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET archived = 1, updated = ?2 WHERE id = ?1",
            rusqlite::params![id, timestamp()],
        )?;
        Ok(())
    }

    pub fn unembedded(&self, model: &str) -> Result<Vec<(i64, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT m.id, m.title || char(10) || m.body FROM memories m
             LEFT JOIN embeddings e ON e.memory_id = m.id AND e.model = ?1
             WHERE m.archived = 0 AND e.memory_id IS NULL",
        )?;
        let rows = statement.query_map([model], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn last_used(&self, kind: &str, name: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT MAX(used_at) FROM usage WHERE kind = ?1 AND name = ?2",
                rusqlite::params![kind, name],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
    }

    pub fn add_generated_skill(&self, name: &str, description: &str, instructions: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO generated_skills (name, description, instructions, created) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (name) DO UPDATE SET description = ?2, instructions = ?3, archived = 0",
            rusqlite::params![name, description, instructions, timestamp()],
        )?;
        Ok(())
    }

    pub fn generated_skills(&self) -> Result<Vec<GeneratedSkill>> {
        let mut statement = self.connection.prepare(
            "SELECT name, description, instructions, created FROM generated_skills WHERE archived = 0",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(GeneratedSkill {
                name: row.get(0)?,
                description: row.get(1)?,
                instructions: row.get(2)?,
                created: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn archive_generated_skill(&self, name: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE generated_skills SET archived = 1 WHERE name = ?1",
            [name],
        )?;
        Ok(())
    }

    pub fn state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM curator_state WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .ok())
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO curator_state (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = ?2",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    pub fn clear_state(&self, key: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM curator_state WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn cursor(&self, transcript_path: &str) -> Result<u64> {
        let offset = self
            .connection
            .query_row(
                "SELECT byte_offset FROM cursors WHERE transcript_path = ?1",
                [transcript_path],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0);
        Ok(offset as u64)
    }

    pub fn set_cursor(&self, transcript_path: &str, byte_offset: u64) -> Result<()> {
        self.connection.execute(
            "INSERT INTO cursors (transcript_path, byte_offset, updated) VALUES (?1, ?2, ?3)
             ON CONFLICT (transcript_path) DO UPDATE SET byte_offset = ?2, updated = ?3",
            rusqlite::params![transcript_path, byte_offset as i64, timestamp()],
        )?;
        Ok(())
    }
}

fn row_to_stored(row: &rusqlite::Row) -> rusqlite::Result<Stored> {
    Ok(Stored {
        id: row.get(0)?,
        kind: row.get(1)?,
        entity: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        pinned: row.get(5)?,
        archived: row.get(6)?,
        updated: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts5_is_available_in_the_bundled_build() {
        let memory = Memory::open_in_memory().unwrap();
        memory
            .connection
            .execute_batch("CREATE VIRTUAL TABLE probe USING fts5(content)")
            .unwrap();
    }

    #[test]
    fn adding_and_linking_memories() {
        let memory = Memory::open_in_memory().unwrap();
        let first = memory
            .add(&NewObservation {
                kind: Kind::Observation,
                entity: Some("project:ax".into()),
                title: "ax uses flocks".into(),
                body: "Locking follows the proper-lockfile protocol.".into(),
                links: vec![],
                source_session: None,
            })
            .unwrap();
        let second = memory
            .add(&NewObservation {
                kind: Kind::Observation,
                entity: None,
                title: "agent reuses ax idioms".into(),
                body: "See [[ax uses flocks]].".into(),
                links: vec!["ax uses flocks".into()],
                source_session: Some("s1".into()),
            })
            .unwrap();

        let neighbors_of_first = memory.neighbors(first).unwrap();
        assert_eq!(neighbors_of_first.len(), 1);
        assert_eq!(neighbors_of_first[0].id, second);

        let neighbors_of_second = memory.neighbors(second).unwrap();
        assert_eq!(neighbors_of_second.len(), 1);
        assert_eq!(neighbors_of_second[0].title, "ax uses flocks");
    }

    #[test]
    fn cursors_start_at_zero_and_persist() {
        let memory = Memory::open_in_memory().unwrap();
        assert_eq!(memory.cursor("/tmp/t.jsonl").unwrap(), 0);
        memory.set_cursor("/tmp/t.jsonl", 4096).unwrap();
        memory.set_cursor("/tmp/t.jsonl", 8192).unwrap();
        assert_eq!(memory.cursor("/tmp/t.jsonl").unwrap(), 8192);
    }
}
