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

use crate::clock::timestamp;

pub struct Memory {
    pub connection: Connection,
}

pub struct NewMemory {
    pub kind: Kind,
    pub entity: Option<String>,
    pub title: String,
    pub body: String,
    pub links: Vec<String>,
    pub source_session: Option<String>,
    pub class: Option<String>,
}

/// Why an observation was captured — and how long it deserves to live. A
/// preference that hasn't come up in months is still a preference; only
/// history and reference age out for lack of retrieval.
pub const CLASSES: [&str; 7] = [
    "preference",
    "constraint",
    "identity",
    "relationship",
    "decision",
    "history",
    "reference",
];

pub const RETIRABLE_CLASSES: [&str; 2] = ["history", "reference"];

#[derive(PartialEq, Clone, Copy)]
pub enum Kind {
    Observation,
    Card,
    Status,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Observation => "observation",
            Kind::Card => "card",
            Kind::Status => "status",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

impl rusqlite::types::FromSql for Kind {
    fn column_result(value: rusqlite::types::ValueRef) -> rusqlite::types::FromSqlResult<Self> {
        match value.as_str()? {
            "observation" => Ok(Kind::Observation),
            "card" => Ok(Kind::Card),
            "status" => Ok(Kind::Status),
            other => Err(rusqlite::types::FromSqlError::Other(
                format!("unknown memory kind '{other}'").into(),
            )),
        }
    }
}

const MIGRATE_2_TO_3: &str = "
    ALTER TABLE memories ADD COLUMN class TEXT;
    ALTER TABLE memories ADD COLUMN archive_reason TEXT;

    CREATE TABLE memory_deliveries (
        id INTEGER PRIMARY KEY,
        memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
        session_id TEXT NOT NULL,
        event TEXT NOT NULL,
        form TEXT NOT NULL,
        delivered_at TEXT NOT NULL
    );
    CREATE INDEX deliveries_by_memory ON memory_deliveries(memory_id, delivered_at);

    CREATE TABLE memory_evidence (
        memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
        source_session TEXT,
        turn_id TEXT NOT NULL,
        role TEXT NOT NULL,
        excerpt TEXT NOT NULL
    );

    CREATE TABLE review_chunks (
        id INTEGER PRIMARY KEY,
        transcript_path TEXT NOT NULL,
        start_offset INTEGER NOT NULL,
        end_offset INTEGER NOT NULL,
        source_session TEXT,
        project_entity TEXT,
        turns TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending',
        attempts INTEGER NOT NULL DEFAULT 0,
        lease_until TEXT,
        next_attempt TEXT,
        last_error TEXT,
        created TEXT NOT NULL,
        UNIQUE(transcript_path, start_offset, end_offset)
    );

    CREATE TABLE entity_aliases (
        alias TEXT PRIMARY KEY,
        canonical_entity TEXT NOT NULL,
        last_seen TEXT NOT NULL
    );

    PRAGMA user_version = 3;
";

/// v4 adds string cursor tokens (opencode addresses turns by message id, not
/// byte offset) and rebuilds review_chunks without the offset key — the
/// atomic cursor advance is what keeps a span from re-queuing, so the offsets
/// carried no weight.
const MIGRATE_3_TO_4: &str = "
    ALTER TABLE cursors ADD COLUMN token TEXT;

    CREATE TABLE review_chunks_v4 (
        id INTEGER PRIMARY KEY,
        transcript_path TEXT NOT NULL,
        source_session TEXT,
        project_entity TEXT,
        turns TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending',
        attempts INTEGER NOT NULL DEFAULT 0,
        lease_until TEXT,
        next_attempt TEXT,
        last_error TEXT,
        created TEXT NOT NULL
    );
    INSERT INTO review_chunks_v4
        (id, transcript_path, source_session, project_entity, turns,
         status, attempts, lease_until, next_attempt, last_error, created)
    SELECT id, transcript_path, source_session, project_entity, turns,
           status, attempts, lease_until, next_attempt, last_error, created
    FROM review_chunks;
    DROP TABLE review_chunks;
    ALTER TABLE review_chunks_v4 RENAME TO review_chunks;

    PRAGMA user_version = 4;
";

pub struct NewReviewChunk {
    pub transcript_path: String,
    pub source_session: Option<String>,
    pub project_entity: Option<String>,
    pub turns: String,
}

pub struct ReviewChunk {
    pub id: i64,
    pub source_session: Option<String>,
    pub project_entity: Option<String>,
    pub turns: String,
    pub attempts: i64,
}

pub struct OverviewRow {
    pub stored: Stored,
    pub uses: i64,
    pub last_used: Option<String>,
}

#[derive(Clone, Copy)]
pub enum ListFilter {
    Active,
    All,
    ArchivedOnly,
}

pub struct GeneratedSkill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub created: String,
}

pub struct Stored {
    pub id: i64,
    pub kind: Kind,
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
        // The version check and the schema creation share one write
        // transaction — a supervisor and a freshly spawned reviewer can both
        // open a brand-new store at the same moment
        // Steps run in order and each re-reads the version, so a store at any
        // age walks the whole chain in one open.
        self.with_transaction(|memory| {
            if memory.schema_version()? == 0 {
                return memory.create_schema();
            }
            if memory.schema_version()? == 1 {
                // The table held reviewer state from day one — the old name lied
                memory.connection.execute_batch(
                    "ALTER TABLE curator_state RENAME TO state; PRAGMA user_version = 2;",
                )?;
            }
            if memory.schema_version()? == 2 {
                memory.connection.execute_batch(MIGRATE_2_TO_3)?;
            }
            if memory.schema_version()? == 3 {
                memory.connection.execute_batch(MIGRATE_3_TO_4)?;
            }
            Ok(())
        })
    }

    fn schema_version(&self) -> Result<i64> {
        Ok(self
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// BEGIN IMMEDIATE around `work`; COMMIT on success, best-effort ROLLBACK
    /// on failure so an error never strands an open transaction on the shared
    /// connection.
    pub fn with_transaction<T>(&self, work: impl FnOnce(&Memory) -> Result<T>) -> Result<T> {
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        match work(self) {
            Ok(value) => {
                self.connection.execute_batch("COMMIT")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self.connection.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn create_schema(&self) -> Result<()> {
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
                token TEXT,
                updated TEXT NOT NULL
            );

            CREATE TABLE state (
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

            ALTER TABLE memories ADD COLUMN class TEXT;
            ALTER TABLE memories ADD COLUMN archive_reason TEXT;

            CREATE TABLE memory_deliveries (
                id INTEGER PRIMARY KEY,
                memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                session_id TEXT NOT NULL,
                event TEXT NOT NULL,
                form TEXT NOT NULL,
                delivered_at TEXT NOT NULL
            );
            CREATE INDEX deliveries_by_memory ON memory_deliveries(memory_id, delivered_at);

            CREATE TABLE memory_evidence (
                memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
                source_session TEXT,
                turn_id TEXT NOT NULL,
                role TEXT NOT NULL,
                excerpt TEXT NOT NULL
            );

            CREATE TABLE review_chunks (
                id INTEGER PRIMARY KEY,
                transcript_path TEXT NOT NULL,
                source_session TEXT,
                project_entity TEXT,
                turns TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                attempts INTEGER NOT NULL DEFAULT 0,
                lease_until TEXT,
                next_attempt TEXT,
                last_error TEXT,
                created TEXT NOT NULL
            );

            CREATE TABLE entity_aliases (
                alias TEXT PRIMARY KEY,
                canonical_entity TEXT NOT NULL,
                last_seen TEXT NOT NULL
            );

            PRAGMA user_version = 4;
            ",
        )?;
        Ok(())
    }

    pub fn add(&self, memory: &NewMemory) -> Result<i64> {
        let now = timestamp();
        self.connection.execute(
            "INSERT INTO memories (kind, entity, title, body, created, updated, source_session, class)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7)",
            rusqlite::params![
                memory.kind.as_str(),
                memory.entity,
                memory.title,
                memory.body,
                now,
                memory.source_session,
                memory.class,
            ],
        )?;
        let id = self.connection.last_insert_rowid();

        for title in &memory.links {
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
             WHERE e.model = ?1 AND m.archived = 0 AND m.kind != 'status'",
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

    pub fn status_for_entity(&self, entity: &str) -> Result<Option<Stored>> {
        let status = self
            .connection
            .query_row(
                "SELECT id, kind, entity, title, body, pinned, archived, updated
                 FROM memories
                 WHERE archived = 0 AND kind = 'status' AND entity = ?1",
                [entity],
                row_to_stored,
            )
            .ok();
        Ok(status)
    }

    /// Status is overwrite-by-entity: one row per project, always the newest
    /// picture, never accumulated.
    pub fn upsert_status(&self, entity: &str, body: &str) -> Result<()> {
        if let Some(existing) = self.status_for_entity(entity)? {
            self.update_body(existing.id, body)
        } else {
            self.add(&NewMemory {
                kind: Kind::Status,
                entity: Some(entity.to_string()),
                title: format!("Current state of {}", crate::cards::entity_name(entity)),
                body: body.to_string(),
                links: Vec::new(),
                source_session: None,
                class: None,
            })
            .map(|_| ())
        }
    }

    pub fn entities(&self) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT entity FROM memories
             WHERE archived = 0 AND entity IS NOT NULL ORDER BY entity",
        )?;
        let rows = statement.query_map([], |row| row.get(0))?;
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

    pub fn archive(&self, id: i64, reason: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET archived = 1, archive_reason = ?2, updated = ?3 WHERE id = ?1",
            rusqlite::params![id, reason, timestamp()],
        )?;
        Ok(())
    }

    pub fn record_delivery(&self, memory_id: i64, session_id: &str, event: &str, form: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO memory_deliveries (memory_id, session_id, event, form, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![memory_id, session_id, event, form, timestamp()],
        )?;
        Ok(())
    }

    pub fn add_evidence(
        &self,
        memory_id: i64,
        source_session: Option<&str>,
        turn_id: &str,
        role: &str,
        excerpt: &str,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO memory_evidence (memory_id, source_session, turn_id, role, excerpt)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![memory_id, source_session, turn_id, role, excerpt],
        )?;
        Ok(())
    }

    pub fn record_alias(&self, alias: &str, canonical_entity: &str) -> Result<()> {
        if alias == canonical_entity {
            return Ok(());
        }
        self.connection.execute(
            "INSERT INTO entity_aliases (alias, canonical_entity, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT (alias) DO UPDATE SET canonical_entity = ?2, last_seen = ?3",
            rusqlite::params![alias, canonical_entity, timestamp()],
        )?;
        Ok(())
    }

    /// Memories filed under a path that later resolved to a canonical project
    /// root get moved home, so worktrees and symlinks stop splitting memory.
    pub fn rehome_aliased_entities(&self) -> Result<usize> {
        let moved = self.connection.execute(
            "UPDATE memories SET entity = (
                 SELECT canonical_entity FROM entity_aliases WHERE alias = memories.entity
             )
             WHERE entity IN (SELECT alias FROM entity_aliases)",
            [],
        )?;
        Ok(moved)
    }

    pub fn unarchive(&self, id: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET archived = 0, updated = ?2 WHERE id = ?1",
            rusqlite::params![id, timestamp()],
        )?;
        Ok(())
    }

    /// The listing the CLI shows: every row with its lifetime injection count
    /// and when it was last pulled into a session.
    pub fn overview(&self, filter: ListFilter) -> Result<Vec<OverviewRow>> {
        let condition = match filter {
            ListFilter::Active => "m.archived = 0",
            ListFilter::All => "1 = 1",
            ListFilter::ArchivedOnly => "m.archived = 1",
        };
        let mut statement = self.connection.prepare(&format!(
            "SELECT m.id, m.kind, m.entity, m.title, m.body, m.pinned, m.archived, m.updated,
                    (SELECT COUNT(*) FROM memory_deliveries d WHERE d.memory_id = m.id),
                    (SELECT MAX(delivered_at) FROM memory_deliveries d WHERE d.memory_id = m.id)
             FROM memories m WHERE {condition} ORDER BY m.updated DESC"
        ))?;
        let rows = statement.query_map([], |row| {
            Ok(OverviewRow {
                stored: row_to_stored(row)?,
                uses: row.get(8)?,
                last_used: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update(&self, id: i64, title: &str, body: &str, entity: Option<&str>) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET title = ?2, body = ?3, entity = ?4, updated = ?5 WHERE id = ?1",
            rusqlite::params![id, title, body, entity, timestamp()],
        )?;
        Ok(())
    }

    pub fn replace_links(&self, id: i64, links: &[String]) -> Result<()> {
        self.connection
            .execute("DELETE FROM links WHERE from_id = ?1", [id])?;
        for title in links {
            self.connection.execute(
                "INSERT OR IGNORE INTO links (from_id, to_title) VALUES (?1, ?2)",
                rusqlite::params![id, title],
            )?;
        }
        Ok(())
    }

    pub fn unembedded(&self, model: &str) -> Result<Vec<(i64, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT m.id, m.title || char(10) || m.body FROM memories m
             LEFT JOIN embeddings e ON e.memory_id = m.id AND e.model = ?1
             WHERE m.archived = 0 AND m.kind != 'status' AND e.memory_id IS NULL",
        )?;
        let rows = statement.query_map([model], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Observations that were never once delivered into a session — retirement
    /// candidates once they're old enough. Preferences, constraints, and
    /// identity facts are exempt: not having come up yet doesn't make them
    /// trivia. Unclassified rows predate classes and age out like history.
    pub fn unretrieved_observations(&self) -> Result<Vec<Stored>> {
        let retirable = RETIRABLE_CLASSES
            .iter()
            .map(|it| format!("'{it}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = self.connection.prepare(&format!(
            "SELECT m.id, m.kind, m.entity, m.title, m.body, m.pinned, m.archived, m.updated
             FROM memories m
             WHERE m.archived = 0 AND m.kind = 'observation' AND m.pinned = 0
               AND (m.class IS NULL OR m.class IN ({retirable}))
               AND NOT EXISTS (
                 SELECT 1 FROM memory_deliveries d WHERE d.memory_id = m.id
               )"
        ))?;
        let rows = statement.query_map([], row_to_stored)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn enqueue_review_chunk(&self, chunk: &NewReviewChunk) -> Result<()> {
        self.connection.execute(
            "INSERT INTO review_chunks
                 (transcript_path, source_session, project_entity, turns, created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                chunk.transcript_path,
                chunk.source_session,
                chunk.project_entity,
                chunk.turns,
                timestamp(),
            ],
        )?;
        Ok(())
    }

    /// Leases the oldest chunk that's due: pending with no future retry time,
    /// or whose lease expired (a crashed reviewer drops its work back into
    /// the pool). The lease keeps two drainers off the same chunk.
    pub fn lease_review_chunk(&self, lease_minutes: u64) -> Result<Option<ReviewChunk>> {
        let now = timestamp();
        let chunk = self
            .connection
            .query_row(
                "SELECT id, source_session, project_entity, turns, attempts FROM review_chunks
                 WHERE status = 'pending'
                   AND (next_attempt IS NULL OR next_attempt <= ?1)
                   AND (lease_until IS NULL OR lease_until <= ?1)
                 ORDER BY created ASC LIMIT 1",
                [&now],
                |row| {
                    Ok(ReviewChunk {
                        id: row.get(0)?,
                        source_session: row.get(1)?,
                        project_entity: row.get(2)?,
                        turns: row.get(3)?,
                        attempts: row.get(4)?,
                    })
                },
            )
            .ok();

        if let Some(chunk) = &chunk {
            let lease_until = crate::clock::timestamp_in(lease_minutes * 60);
            self.connection.execute(
                "UPDATE review_chunks SET lease_until = ?2 WHERE id = ?1",
                rusqlite::params![chunk.id, lease_until],
            )?;
        }
        Ok(chunk)
    }

    pub fn complete_review_chunk(&self, id: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE review_chunks SET status = 'done', lease_until = NULL WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// A failed chunk goes back in the pool with backoff; after enough
    /// attempts it's kept as `dead` for inspection, never deleted.
    pub fn fail_review_chunk(&self, id: i64, attempts: i64, error: &str, cap: i64) -> Result<()> {
        if attempts + 1 >= cap {
            self.connection.execute(
                "UPDATE review_chunks
                 SET status = 'dead', attempts = ?2, last_error = ?3, lease_until = NULL
                 WHERE id = ?1",
                rusqlite::params![id, attempts + 1, error],
            )?;
        } else {
            let backoff_seconds = (attempts as u64 + 1) * 600;
            self.connection.execute(
                "UPDATE review_chunks
                 SET attempts = ?2, last_error = ?3, next_attempt = ?4, lease_until = NULL
                 WHERE id = ?1",
                rusqlite::params![id, attempts + 1, error, crate::clock::timestamp_in(backoff_seconds)],
            )?;
        }
        Ok(())
    }

    pub fn prune_cursors(&self, keep: impl Fn(&str) -> bool) -> Result<()> {
        let mut statement = self
            .connection
            .prepare("SELECT transcript_path FROM cursors")?;
        let paths: Vec<String> = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for path in paths.iter().filter(|it| !keep(it)) {
            self.connection
                .execute("DELETE FROM cursors WHERE transcript_path = ?1", [path])?;
        }
        Ok(())
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
                "SELECT value FROM state WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .ok())
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO state (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = ?2",
            rusqlite::params![key, value],
        )?;
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

    /// opencode's cursor is the last message id consumed, not a byte offset;
    /// it shares the `cursors` table so `prune_cursors` covers both kinds.
    pub fn cursor_token(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT token FROM cursors WHERE transcript_path = ?1",
                [key],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
    }

    pub fn set_cursor_token(&self, key: &str, token: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO cursors (transcript_path, byte_offset, token, updated) VALUES (?1, 0, ?2, ?3)
             ON CONFLICT (transcript_path) DO UPDATE SET token = ?2, updated = ?3",
            rusqlite::params![key, token, timestamp()],
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
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: Some("project:ax".into()),
                title: "ax uses flocks".into(),
                body: "Locking follows the proper-lockfile protocol.".into(),
                links: vec![],
                source_session: None,
                class: None,
            })
            .unwrap();
        let second = memory
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: None,
                title: "agent reuses ax idioms".into(),
                body: "See [[ax uses flocks]].".into(),
                links: vec!["ax uses flocks".into()],
                source_session: Some("s1".into()),
                class: None,
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
    fn status_is_one_overwritten_row_per_entity() {
        let memory = Memory::open_in_memory().unwrap();
        memory.upsert_status("project:/x", "PR 1 open.").unwrap();
        memory.upsert_status("project:/x", "PR 1 merged, PR 2 open.").unwrap();

        let status = memory.status_for_entity("project:/x").unwrap().unwrap();
        assert_eq!(status.body, "PR 1 merged, PR 2 open.");
        let statuses: Vec<_> = memory
            .list()
            .unwrap()
            .into_iter()
            .filter(|it| it.kind == Kind::Status)
            .collect();
        assert_eq!(statuses.len(), 1);
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
