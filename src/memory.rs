//! The memory store: everything agent learns, in one SQLite database.
//!
//! There is a single shared store — memories belong to the person, not to
//! whichever account or session produced them. The database is the source of
//! truth; the markdown cards under `memory/cards/` are rendered views of it.
//! WAL mode plus a busy timeout lets concurrent sessions write without
//! coordination; the reviewer and curator add their own flocks on top so
//! only one of each runs at a time.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::clock::timestamp;
use crate::id::Id;
use crate::project::Project;

pub struct Memory {
    pub connection: Connection,
    directory: Option<PathBuf>,
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

#[derive(PartialEq, Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Observation,
    Card,
    Status,
}

impl Kind {
    pub fn parse(name: &str) -> Option<Kind> {
        match name {
            "observation" => Some(Kind::Observation),
            "card" => Some(Kind::Card),
            "status" => Some(Kind::Status),
            _ => None,
        }
    }

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

/// v5 keeps every relevance judgment the reranker makes, rejected candidates
/// included — prompt, memory, and logit are the raw material for fine-tuning
/// a judge on this store's own traffic.
const MIGRATE_4_TO_5: &str = "
    CREATE TABLE relevance_judgments (
        id INTEGER PRIMARY KEY,
        memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
        session_id TEXT NOT NULL,
        prompt TEXT NOT NULL,
        model TEXT NOT NULL,
        logit REAL NOT NULL,
        judged_at TEXT NOT NULL
    );

    PRAGMA user_version = 5;
";

/// v6 remembers each project's root commit — the proof, when a checkout's
/// remote changes, that the old name and the new one are the same repository.
const MIGRATE_5_TO_6: &str = "
    CREATE TABLE project_roots (
        entity TEXT PRIMARY KEY,
        root_commit TEXT NOT NULL
    );

    PRAGMA user_version = 6;
";

/// Every table keyed by a memory's id. `local_row` is SQLite's rowid made
/// explicit so a VACUUM can't renumber it underneath the full-text index; it
/// also keeps the order memories were learned in. It never leaves this store.
const MEMORY_TABLES: &str = "
    CREATE TABLE memories (
        local_row INTEGER PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        kind TEXT NOT NULL,
        class TEXT,
        entity TEXT,
        title TEXT NOT NULL,
        body TEXT NOT NULL,
        created TEXT NOT NULL,
        updated TEXT NOT NULL,
        source_session TEXT,
        pinned INTEGER NOT NULL DEFAULT 0,
        archived INTEGER NOT NULL DEFAULT 0,
        archive_reason TEXT
    );

    CREATE VIRTUAL TABLE memories_fts USING fts5(
        title, body, entity,
        content='memories', content_rowid='local_row'
    );

    CREATE TRIGGER memories_insert AFTER INSERT ON memories BEGIN
        INSERT INTO memories_fts(rowid, title, body, entity)
        VALUES (new.local_row, new.title, new.body, new.entity);
    END;
    CREATE TRIGGER memories_delete AFTER DELETE ON memories BEGIN
        INSERT INTO memories_fts(memories_fts, rowid, title, body, entity)
        VALUES ('delete', old.local_row, old.title, old.body, old.entity);
    END;
    CREATE TRIGGER memories_update AFTER UPDATE OF title, body, entity ON memories BEGIN
        INSERT INTO memories_fts(memories_fts, rowid, title, body, entity)
        VALUES ('delete', old.local_row, old.title, old.body, old.entity);
        INSERT INTO memories_fts(rowid, title, body, entity)
        VALUES (new.local_row, new.title, new.body, new.entity);
    END;

    CREATE TABLE links (
        from_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
        to_title TEXT NOT NULL,
        PRIMARY KEY (from_id, to_title)
    );

    CREATE TABLE embeddings (
        memory_id TEXT PRIMARY KEY REFERENCES memories(id) ON DELETE CASCADE,
        model TEXT NOT NULL,
        vector BLOB NOT NULL
    );

    CREATE TABLE memory_deliveries (
        id INTEGER PRIMARY KEY,
        memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
        session_id TEXT NOT NULL,
        event TEXT NOT NULL,
        form TEXT NOT NULL,
        delivered_at TEXT NOT NULL
    );
    CREATE INDEX deliveries_by_memory ON memory_deliveries(memory_id, delivered_at);

    CREATE TABLE memory_evidence (
        memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
        source_session TEXT,
        turn_id TEXT NOT NULL,
        role TEXT NOT NULL,
        excerpt TEXT NOT NULL
    );

    CREATE TABLE relevance_judgments (
        id INTEGER PRIMARY KEY,
        memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
        session_id TEXT NOT NULL,
        prompt TEXT NOT NULL,
        model TEXT NOT NULL,
        logit REAL NOT NULL,
        judged_at TEXT NOT NULL
    );
";

/// v8 is what lets stores on different machines share memories without one
/// of them being in charge.
///
/// Every write gets a dot — this store's node id and the next tick of its
/// clock — and folds that dot into the row's version vector. Comparing two
/// vectors says whether one version grew out of the other or whether two
/// machines changed the same memory at once. The stamping lives in triggers,
/// not in the methods that write, so no write path can forget it; a write
/// that arrives from a peer carries its own dot, and the triggers leave any
/// row whose dot changed alone.
///
/// `sync_knowledge` is how far this store has caught up with each node,
/// `memory_siblings` holds versions that conflict with the local one until
/// they're merged, and `merged_from` marks a version as a merge of two
/// others so that two machines merging the same pair don't then have to
/// merge their merges. A link is part of its memory, so changing one touches
/// the memory and stamps it.
const MIGRATE_7_TO_8: &str = "
    ALTER TABLE memories ADD COLUMN dot_node TEXT;
    ALTER TABLE memories ADD COLUMN dot_seq INTEGER;
    ALTER TABLE memories ADD COLUMN version TEXT NOT NULL DEFAULT '{}';
    ALTER TABLE memories ADD COLUMN merged_from TEXT;
    CREATE INDEX memories_by_dot ON memories(dot_node, dot_seq);

    CREATE TABLE sync_clock (
        node TEXT NOT NULL,
        seq INTEGER NOT NULL
    );

    CREATE TABLE sync_knowledge (
        node TEXT PRIMARY KEY,
        seq INTEGER NOT NULL
    );

    CREATE TABLE memory_siblings (
        memory_id TEXT NOT NULL,
        dot_node TEXT NOT NULL,
        dot_seq INTEGER NOT NULL,
        record TEXT NOT NULL,
        received TEXT NOT NULL,
        PRIMARY KEY (memory_id, dot_node, dot_seq)
    );

    CREATE TABLE peers (
        node TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        address TEXT NOT NULL,
        token TEXT,
        removed INTEGER NOT NULL DEFAULT 0,
        updated TEXT NOT NULL,
        last_synced TEXT,
        marks_sent TEXT
    );

    CREATE TABLE shared_state (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL,
        updated TEXT NOT NULL,
        node TEXT NOT NULL
    );

    CREATE TABLE usage_marks (
        memory_id TEXT NOT NULL,
        node TEXT NOT NULL,
        last_delivered TEXT NOT NULL,
        learned TEXT NOT NULL,
        PRIMARY KEY (memory_id, node)
    );
    CREATE INDEX usage_marks_by_learned ON usage_marks(learned);

    CREATE TABLE pairings (
        code TEXT PRIMARY KEY,
        node TEXT NOT NULL,
        name TEXT NOT NULL,
        address TEXT NOT NULL,
        token TEXT,
        requested TEXT NOT NULL
    );
";

const STAMP_TRIGGERS: &str = "
    CREATE TRIGGER memories_stamp_insert AFTER INSERT ON memories
    WHEN new.dot_node IS NULL BEGIN
        UPDATE sync_clock SET seq = seq + 1;
        UPDATE memories
        SET dot_node = (SELECT node FROM sync_clock),
            dot_seq = (SELECT seq FROM sync_clock),
            version = json_set(new.version, '$.' || (SELECT node FROM sync_clock), (SELECT seq FROM sync_clock))
        WHERE local_row = new.local_row;
    END;

    CREATE TRIGGER memories_stamp_update AFTER UPDATE ON memories
    WHEN new.dot_node IS old.dot_node AND new.dot_seq IS old.dot_seq BEGIN
        UPDATE sync_clock SET seq = seq + 1;
        UPDATE memories
        SET dot_node = (SELECT node FROM sync_clock),
            dot_seq = (SELECT seq FROM sync_clock),
            version = json_set(new.version, '$.' || (SELECT node FROM sync_clock), (SELECT seq FROM sync_clock)),
            merged_from = CASE WHEN new.merged_from IS old.merged_from THEN NULL ELSE new.merged_from END
        WHERE local_row = new.local_row;
    END;

    CREATE TRIGGER links_stamp_insert AFTER INSERT ON links BEGIN
        UPDATE memories SET updated = updated WHERE id = new.from_id;
    END;

    CREATE TRIGGER links_stamp_delete AFTER DELETE ON links BEGIN
        UPDATE memories SET updated = updated WHERE id = old.from_id;
    END;
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

/// What `memory list` shows and in what order. The sort is a list of typed
/// keys, never user text, so the ORDER BY is assembled from fixed fragments.
pub struct Listing {
    pub filter: ListFilter,
    pub kinds: Vec<Kind>,
    pub order: Vec<SortKey>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SortKey {
    pub column: SortColumn,
    pub descending: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SortColumn {
    Id,
    Updated,
    Kind,
    Uses,
    LastUsed,
    Title,
}

impl SortColumn {
    fn as_sql(self) -> &'static str {
        match self {
            SortColumn::Id => "m.local_row",
            SortColumn::Updated => "m.updated",
            SortColumn::Kind => "m.kind",
            SortColumn::Uses => "uses",
            SortColumn::LastUsed => "last_used",
            SortColumn::Title => "m.title COLLATE NOCASE",
        }
    }
}

pub struct GeneratedSkill {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub created: String,
}

/// A memory as it travels between stores: everything that is the memory
/// itself, under the id it has everywhere, and nothing about how one store
/// came by it or used it — sessions, evidence, deliveries, and embeddings all
/// stay behind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Portable {
    pub id: Id,
    pub kind: Kind,
    pub class: Option<String>,
    pub entity: Option<String>,
    pub title: String,
    pub body: String,
    pub links: Vec<String>,
    pub pinned: bool,
    pub archived: bool,
    pub archive_reason: Option<String>,
    pub created: String,
    pub updated: String,
}

impl Portable {
    /// Whether two copies are the same memory in every way a person would
    /// notice — which id it has and when it was written aside.
    pub fn says_the_same_as(&self, other: &Portable) -> bool {
        let timeless = |it: &Portable| Portable {
            id: self.id,
            created: String::new(),
            updated: String::new(),
            ..it.clone()
        };
        timeless(self) == timeless(other)
    }
}

pub struct Stored {
    pub id: Id,
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

        let memory = Memory { connection, directory: Some(directory.to_path_buf()) };
        memory.snapshot_before_global_ids(directory)?;
        memory.migrate()?;
        Ok(memory)
    }

    /// Re-identifying every memory rewrites every table that points at one,
    /// and there's no walking it back, so the store as it was is kept beside
    /// the new one. Two processes can open an old store at once; each writes
    /// its own copy and the first to finish keeps the name.
    fn snapshot_before_global_ids(&self, directory: &Path) -> Result<()> {
        let snapshot = directory.join("store-before-global-ids.db");
        if (1..7).contains(&self.schema_version()?) && !snapshot.exists() {
            let scratch = directory.join(format!("store-before-global-ids.{}.tmp", std::process::id()));
            let _ = std::fs::remove_file(&scratch);
            self.connection
                .execute("VACUUM INTO ?1", [scratch.to_string_lossy()])
                .context("could not snapshot the store before migrating it — is the disk full?")?;
            if snapshot.exists() {
                std::fs::remove_file(&scratch)?;
            } else {
                std::fs::rename(&scratch, &snapshot)?;
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Memory> {
        let memory = Memory {
            connection: Connection::open_in_memory()?,
            directory: None,
        };
        memory.migrate()?;
        Ok(memory)
    }

    /// Where this store's cards are rendered for people to read; a store that
    /// lives only in memory has nowhere to put them.
    pub fn cards_dir(&self) -> Option<PathBuf> {
        self.directory.as_ref().map(|it| it.join("cards"))
    }

    fn migrate(&self) -> Result<()> {
        // The version check and the schema creation share one write
        // transaction — a supervisor and a freshly spawned reviewer can both
        // open a brand-new store at the same moment
        // Steps run in order and each re-reads the version, so a store at any
        // age walks the whole chain in one open.
        // A new store is created at v7 and walks the rest of the chain like
        // any other, so later steps are written once, not once per path.
        self.with_transaction(|memory| {
            if memory.schema_version()? == 0 {
                memory.create_schema()?;
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
            if memory.schema_version()? == 4 {
                memory.connection.execute_batch(MIGRATE_4_TO_5)?;
            }
            if memory.schema_version()? == 5 {
                memory.connection.execute_batch(MIGRATE_5_TO_6)?;
            }
            if memory.schema_version()? == 6 {
                memory.migrate_to_global_ids()?;
            }
            if memory.schema_version()? == 7 {
                memory.migrate_to_stamped_writes()?;
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

    /// v7 swaps the counted integer ids for random ones that hold on every
    /// machine. Each memory table is rebuilt around the new ids; the old id
    /// becomes `local_row`, so memories keep the order they were learned in.
    fn migrate_to_global_ids(&self) -> Result<()> {
        self.connection.execute_batch(
            "
            DROP TRIGGER IF EXISTS memories_insert;
            DROP TRIGGER IF EXISTS memories_delete;
            DROP TRIGGER IF EXISTS memories_update;
            DROP TABLE memories_fts;
            DROP INDEX deliveries_by_memory;
            ALTER TABLE memories RENAME TO memories_v6;
            ALTER TABLE links RENAME TO links_v6;
            ALTER TABLE embeddings RENAME TO embeddings_v6;
            ALTER TABLE memory_deliveries RENAME TO memory_deliveries_v6;
            ALTER TABLE memory_evidence RENAME TO memory_evidence_v6;
            ALTER TABLE relevance_judgments RENAME TO relevance_judgments_v6;
            CREATE TABLE new_ids (old INTEGER PRIMARY KEY, new TEXT NOT NULL UNIQUE);
            ",
        )?;
        self.connection.execute_batch(MEMORY_TABLES)?;

        let old_ids: Vec<i64> = self
            .connection
            .prepare("SELECT id FROM memories_v6 ORDER BY id")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for old in old_ids {
            self.connection.execute(
                "INSERT INTO new_ids (old, new) VALUES (?1, ?2)",
                rusqlite::params![old, Id::generate()],
            )?;
        }

        self.connection.execute_batch(
            "
            INSERT INTO memories
                (local_row, id, kind, class, entity, title, body, created, updated,
                 source_session, pinned, archived, archive_reason)
            SELECT m.id, n.new, m.kind, m.class, m.entity, m.title, m.body, m.created, m.updated,
                   m.source_session, m.pinned, m.archived, m.archive_reason
            FROM memories_v6 m JOIN new_ids n ON n.old = m.id ORDER BY m.id;

            INSERT INTO links (from_id, to_title)
            SELECT n.new, l.to_title FROM links_v6 l JOIN new_ids n ON n.old = l.from_id;

            INSERT INTO embeddings (memory_id, model, vector)
            SELECT n.new, e.model, e.vector FROM embeddings_v6 e JOIN new_ids n ON n.old = e.memory_id;

            INSERT INTO memory_deliveries (memory_id, session_id, event, form, delivered_at)
            SELECT n.new, d.session_id, d.event, d.form, d.delivered_at
            FROM memory_deliveries_v6 d JOIN new_ids n ON n.old = d.memory_id ORDER BY d.id;

            INSERT INTO memory_evidence (memory_id, source_session, turn_id, role, excerpt)
            SELECT n.new, e.source_session, e.turn_id, e.role, e.excerpt
            FROM memory_evidence_v6 e JOIN new_ids n ON n.old = e.memory_id;

            INSERT INTO relevance_judgments (memory_id, session_id, prompt, model, logit, judged_at)
            SELECT n.new, j.session_id, j.prompt, j.model, j.logit, j.judged_at
            FROM relevance_judgments_v6 j JOIN new_ids n ON n.old = j.memory_id ORDER BY j.id;

            DROP TABLE links_v6;
            DROP TABLE embeddings_v6;
            DROP TABLE memory_deliveries_v6;
            DROP TABLE memory_evidence_v6;
            DROP TABLE relevance_judgments_v6;
            DROP TABLE memories_v6;
            DROP TABLE new_ids;

            PRAGMA user_version = 7;
            ",
        )?;
        Ok(())
    }

    /// Every memory already here becomes this node's write, stamped in the
    /// order it was learned, before the triggers take over.
    fn migrate_to_stamped_writes(&self) -> Result<()> {
        self.connection.execute_batch(MIGRATE_7_TO_8)?;
        self.connection.execute(
            "INSERT INTO sync_clock (node, seq) VALUES (?1, 0)",
            [Id::generate()],
        )?;
        self.connection.execute_batch(
            "
            UPDATE memories
            SET dot_node = (SELECT node FROM sync_clock),
                dot_seq = local_row,
                version = json_object((SELECT node FROM sync_clock), local_row);
            UPDATE sync_clock SET seq = COALESCE((SELECT MAX(local_row) FROM memories), 0);

            INSERT INTO usage_marks (memory_id, node, last_delivered, learned)
            SELECT memory_id, (SELECT node FROM sync_clock), substr(MAX(delivered_at), 1, 10), MAX(delivered_at)
            FROM memory_deliveries GROUP BY memory_id;
            ",
        )?;
        self.connection.execute_batch(STAMP_TRIGGERS)?;
        self.connection.execute_batch("PRAGMA user_version = 8;")?;
        Ok(())
    }

    fn create_schema(&self) -> Result<()> {
        self.connection.execute_batch(MEMORY_TABLES)?;
        self.connection.execute_batch(
            "
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

            CREATE TABLE project_roots (
                entity TEXT PRIMARY KEY,
                root_commit TEXT NOT NULL
            );

            PRAGMA user_version = 7;
            ",
        )?;
        Ok(())
    }

    pub fn add(&self, memory: &NewMemory) -> Result<Id> {
        let id = Id::generate();
        let now = timestamp();
        self.connection.execute(
            "INSERT INTO memories (id, kind, entity, title, body, created, updated, source_session, class)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8)",
            rusqlite::params![
                id,
                memory.kind.as_str(),
                memory.entity,
                memory.title,
                memory.body,
                now,
                memory.source_session,
                memory.class,
            ],
        )?;

        for title in &memory.links {
            self.connection.execute(
                "INSERT OR IGNORE INTO links (from_id, to_title) VALUES (?1, ?2)",
                rusqlite::params![id, title],
            )?;
        }
        Ok(id)
    }

    pub fn get(&self, id: Id) -> Result<Stored> {
        self.connection
            .query_row(
                "SELECT id, kind, entity, title, body, pinned, archived, updated
                 FROM memories WHERE id = ?1",
                [id],
                row_to_stored,
            )
            .with_context(|| format!("no memory with id {id} — see `katami memory list`"))
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
    pub fn neighbors(&self, id: Id) -> Result<Vec<Stored>> {
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

    pub fn set_embedding(&self, id: Id, model: &str, vector: &[f32]) -> Result<()> {
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

    pub fn embeddings(&self, model: &str) -> Result<Vec<(Id, Vec<f32>)>> {
        let mut statement = self.connection.prepare(
            "SELECT e.memory_id, e.vector FROM embeddings e
             JOIN memories m ON m.id = e.memory_id
             WHERE e.model = ?1 AND m.archived = 0 AND m.kind != 'status'",
        )?;
        let rows = statement.query_map([model], |row| {
            let id: Id = row.get(0)?;
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

    pub fn update_body(&self, id: Id, body: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET body = ?2, updated = ?3 WHERE id = ?1",
            rusqlite::params![id, body, timestamp()],
        )?;
        Ok(())
    }

    pub fn archive(&self, id: Id, reason: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET archived = 1, archive_reason = ?2, updated = ?3 WHERE id = ?1",
            rusqlite::params![id, reason, timestamp()],
        )?;
        Ok(())
    }

    /// The delivery log stays on this machine; that the memory got used at
    /// all is marked for the mesh, since that's what keeps it from being
    /// retired by a machine that never works on this project.
    pub fn record_delivery(&self, memory_id: Id, session_id: &str, event: &str, form: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO memory_deliveries (memory_id, session_id, event, form, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![memory_id, session_id, event, form, timestamp()],
        )?;
        self.mark_used(memory_id)
    }

    pub fn record_judgment(
        &self,
        memory_id: Id,
        session_id: &str,
        prompt: &str,
        model: &str,
        logit: f32,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO relevance_judgments (memory_id, session_id, prompt, model, logit, judged_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![memory_id, session_id, prompt, model, logit as f64, timestamp()],
        )?;
        Ok(())
    }

    pub fn add_evidence(
        &self,
        memory_id: Id,
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

    /// Files every path the project was reached through under its name, and
    /// moves the memories home right away — the session that's starting is
    /// about to look them up. When a path used to lead to a different name
    /// and the root commit says it's the same repository, the old name
    /// becomes an alias too: that's a rename, not a reused directory.
    pub fn settle_project(&self, project: &Project) -> Result<()> {
        self.with_transaction(|memory| {
            for alias in &project.aliases {
                if let Some(root_commit) = &project.root_commit
                    && let Some(previous) = memory.canonical_entity_for(alias)?
                    && previous != project.entity
                    && memory.root_commit_of(&previous)?.as_ref() == Some(root_commit)
                {
                    memory.record_alias(&previous, &project.entity)?;
                }
                memory.record_alias(alias, &project.entity)?;
            }

            if let Some(root_commit) = &project.root_commit {
                memory.connection.execute(
                    "INSERT INTO project_roots (entity, root_commit) VALUES (?1, ?2)
                     ON CONFLICT (entity) DO UPDATE SET root_commit = ?2",
                    rusqlite::params![project.entity, root_commit],
                )?;
            }
            memory.rehome_aliased_entities()?;
            Ok(())
        })
    }

    /// A canonical name is never itself an alias: whatever pointed at the new
    /// alias now points past it, and the canonical name stops being an alias
    /// for anything, so chains and cycles can't form.
    pub fn record_alias(&self, alias: &str, canonical_entity: &str) -> Result<()> {
        if alias == canonical_entity {
            return Ok(());
        }
        self.connection.execute(
            "INSERT INTO entity_aliases (alias, canonical_entity, last_seen) VALUES (?1, ?2, ?3)
             ON CONFLICT (alias) DO UPDATE SET canonical_entity = ?2, last_seen = ?3",
            rusqlite::params![alias, canonical_entity, timestamp()],
        )?;
        self.connection.execute(
            "UPDATE entity_aliases SET canonical_entity = ?2 WHERE canonical_entity = ?1",
            rusqlite::params![alias, canonical_entity],
        )?;
        self.connection.execute(
            "DELETE FROM entity_aliases WHERE alias = ?1",
            rusqlite::params![canonical_entity],
        )?;
        Ok(())
    }

    fn canonical_entity_for(&self, alias: &str) -> Result<Option<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT canonical_entity FROM entity_aliases WHERE alias = ?1")?;
        let mut rows = statement.query_map([alias], |row| row.get(0))?;
        Ok(rows.next().transpose()?)
    }

    fn root_commit_of(&self, entity: &str) -> Result<Option<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT root_commit FROM project_roots WHERE entity = ?1")?;
        let mut rows = statement.query_map([entity], |row| row.get(0))?;
        Ok(rows.next().transpose()?)
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

    pub fn unarchive(&self, id: Id) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET archived = 0, updated = ?2 WHERE id = ?1",
            rusqlite::params![id, timestamp()],
        )?;
        Ok(())
    }

    /// The listing the CLI shows: every row with its lifetime injection count
    /// and when it was last pulled into a session.
    pub fn overview(&self, listing: &Listing) -> Result<Vec<OverviewRow>> {
        let mut conditions = vec![match listing.filter {
            ListFilter::Active => "m.archived = 0".to_string(),
            ListFilter::All => "1 = 1".to_string(),
            ListFilter::ArchivedOnly => "m.archived = 1".to_string(),
        }];
        if !listing.kinds.is_empty() {
            let kinds: Vec<String> = listing.kinds.iter().map(|it| format!("'{}'", it.as_str())).collect();
            conditions.push(format!("m.kind IN ({})", kinds.join(", ")));
        }

        let mut order: Vec<String> = listing
            .order
            .iter()
            .map(|key| {
                if key.descending {
                    format!("{} DESC", key.column.as_sql())
                } else {
                    format!("{} ASC", key.column.as_sql())
                }
            })
            .collect();
        if order.is_empty() {
            order.push("m.updated DESC".to_string());
        }

        let mut statement = self.connection.prepare(&format!(
            "SELECT m.id, m.kind, m.entity, m.title, m.body, m.pinned, m.archived, m.updated,
                    (SELECT COUNT(*) FROM memory_deliveries d WHERE d.memory_id = m.id) AS uses,
                    (SELECT MAX(delivered_at) FROM memory_deliveries d WHERE d.memory_id = m.id) AS last_used
             FROM memories m WHERE {} ORDER BY {}, m.local_row DESC",
            conditions.join(" AND "),
            order.join(", ")
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

    pub fn update(&self, id: Id, title: &str, body: &str, entity: Option<&str>) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET title = ?2, body = ?3, entity = ?4, updated = ?5 WHERE id = ?1",
            rusqlite::params![id, title, body, entity, timestamp()],
        )?;
        Ok(())
    }

    pub fn replace_links(&self, id: Id, links: &[String]) -> Result<()> {
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

    pub fn ids(&self) -> Result<Vec<Id>> {
        let mut statement = self.connection.prepare("SELECT id FROM memories ORDER BY local_row")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn portable(&self, id: Id) -> Result<Portable> {
        let mut statement = self
            .connection
            .prepare("SELECT to_title FROM links WHERE from_id = ?1 ORDER BY to_title")?;
        let links = statement
            .query_map([id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;

        self.connection
            .query_row(
                "SELECT kind, class, entity, title, body, pinned, archived, archive_reason, created, updated
                 FROM memories WHERE id = ?1",
                [id],
                |row| {
                    Ok(Portable {
                        id,
                        kind: row.get(0)?,
                        class: row.get(1)?,
                        entity: row.get(2)?,
                        title: row.get(3)?,
                        body: row.get(4)?,
                        links,
                        pinned: row.get(5)?,
                        archived: row.get(6)?,
                        archive_reason: row.get(7)?,
                        created: row.get(8)?,
                        updated: row.get(9)?,
                    })
                },
            )
            .with_context(|| format!("no memory with id {id} — see `katami memory list`"))
    }

    /// What a person types for an id: all of it, or just enough of its start
    /// to single one memory out.
    pub fn resolve(&self, typed: &str) -> Result<Id> {
        if typed.is_empty() || !typed.bytes().all(|it| it.is_ascii_alphanumeric()) {
            bail!("`{typed}` is not a memory id — see `katami memory list`");
        }

        let mut statement = self
            .connection
            .prepare("SELECT id FROM memories WHERE id LIKE ?1 || '%' ORDER BY id LIMIT 2")?;
        let matches = statement
            .query_map([typed], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<Id>>>()?;
        match matches.as_slice() {
            [] => bail!("no memory with id {typed} — see `katami memory list`"),
            [only] => Ok(*only),
            _ => bail!("several ids start with {typed} — type more of it"),
        }
    }

    pub fn exists(&self, id: Id) -> Result<bool> {
        Ok(self
            .connection
            .query_row("SELECT EXISTS (SELECT 1 FROM memories WHERE id = ?1)", [id], |row| row.get(0))?)
    }

    /// The memories already here that an arriving one could be another copy
    /// of, live ones first — for a memory that arrives under an id this store
    /// has never seen, from a bundle written by hand or by a store that
    /// learned the same thing separately. Cards and statuses are one per
    /// entity whatever they're titled; observations are the same memory when
    /// title and entity both match. There can be several — superseded
    /// versions stay behind as archived rows.
    pub fn twins_of(&self, portable: &Portable) -> Result<Vec<Id>> {
        let (title_matters, title) = match portable.kind {
            Kind::Observation => (true, portable.title.as_str()),
            Kind::Card | Kind::Status => (false, ""),
        };
        let mut statement = self.connection.prepare(
            "SELECT id FROM memories
             WHERE kind = ?1 AND entity IS ?2 AND (?3 = 0 OR title = ?4)
             ORDER BY archived, local_row",
        )?;
        let rows = statement.query_map(
            rusqlite::params![portable.kind.as_str(), portable.entity, title_matters, title],
            |row| row.get(0),
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn import(&self, portable: &Portable) -> Result<Id> {
        self.connection.execute(
            "INSERT INTO memories
                (id, kind, class, entity, title, body, pinned, archived, archive_reason, created, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                portable.id,
                portable.kind.as_str(),
                portable.class,
                portable.entity,
                portable.title,
                portable.body,
                portable.pinned,
                portable.archived,
                portable.archive_reason,
                portable.created,
                portable.updated,
            ],
        )?;
        self.replace_links(portable.id, &portable.links)?;
        Ok(portable.id)
    }

    pub fn overwrite(&self, id: Id, portable: &Portable) -> Result<()> {
        self.connection.execute(
            "UPDATE memories SET class = ?2, title = ?3, body = ?4, pinned = ?5, archived = ?6,
                                 archive_reason = ?7, updated = ?8, entity = ?9
             WHERE id = ?1",
            rusqlite::params![
                id,
                portable.class,
                portable.title,
                portable.body,
                portable.pinned,
                portable.archived,
                portable.archive_reason,
                portable.updated,
                portable.entity,
            ],
        )?;
        self.replace_links(id, &portable.links)
    }

    pub fn unembedded(&self, model: &str) -> Result<Vec<(Id, String)>> {
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
               )
               AND NOT EXISTS (
                 SELECT 1 FROM usage_marks u WHERE u.memory_id = m.id
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
    fn overview_filters_by_kind_and_sorts_by_typed_keys() {
        let memory = Memory::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for (kind, title) in [
            (Kind::Observation, "quiet"),
            (Kind::Card, "popular"),
            (Kind::Observation, "used once"),
        ] {
            let id = memory
                .add(&NewMemory {
                    kind,
                    entity: None,
                    title: title.into(),
                    body: "Body.".into(),
                    links: vec![],
                    source_session: None,
                    class: None,
                })
                .unwrap();
            ids.push(id);
        }
        memory.record_delivery(ids[1], "s1", "UserPromptSubmit", "full").unwrap();
        memory.record_delivery(ids[1], "s2", "UserPromptSubmit", "full").unwrap();
        memory.record_delivery(ids[2], "s1", "UserPromptSubmit", "full").unwrap();

        let by_uses = memory
            .overview(&Listing {
                filter: ListFilter::Active,
                kinds: vec![],
                order: vec![SortKey { column: SortColumn::Uses, descending: true }],
            })
            .unwrap();
        let titles: Vec<&str> = by_uses.iter().map(|it| it.stored.title.as_str()).collect();
        assert_eq!(titles, vec!["popular", "used once", "quiet"]);
        assert_eq!(by_uses[0].uses, 2);
        assert!(by_uses[2].last_used.is_none());

        let observations = memory
            .overview(&Listing {
                filter: ListFilter::Active,
                kinds: vec![Kind::Observation],
                order: vec![SortKey { column: SortColumn::Title, descending: false }],
            })
            .unwrap();
        let titles: Vec<&str> = observations.iter().map(|it| it.stored.title.as_str()).collect();
        assert_eq!(titles, vec!["quiet", "used once"]);
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

    const V6_STORE: &str = "
        CREATE TABLE memories (
            id INTEGER PRIMARY KEY, kind TEXT NOT NULL, entity TEXT, title TEXT NOT NULL,
            body TEXT NOT NULL, created TEXT NOT NULL, updated TEXT NOT NULL, source_session TEXT,
            pinned INTEGER NOT NULL DEFAULT 0, archived INTEGER NOT NULL DEFAULT 0,
            class TEXT, archive_reason TEXT
        );
        CREATE VIRTUAL TABLE memories_fts USING fts5(title, body, entity, content='memories', content_rowid='id');
        CREATE TRIGGER memories_insert AFTER INSERT ON memories BEGIN
            INSERT INTO memories_fts(rowid, title, body, entity) VALUES (new.id, new.title, new.body, new.entity);
        END;
        CREATE TABLE links (from_id INTEGER NOT NULL, to_title TEXT NOT NULL, PRIMARY KEY (from_id, to_title));
        CREATE TABLE embeddings (memory_id INTEGER PRIMARY KEY, model TEXT NOT NULL, vector BLOB NOT NULL);
        CREATE TABLE memory_deliveries (
            id INTEGER PRIMARY KEY, memory_id INTEGER NOT NULL, session_id TEXT NOT NULL,
            event TEXT NOT NULL, form TEXT NOT NULL, delivered_at TEXT NOT NULL
        );
        CREATE INDEX deliveries_by_memory ON memory_deliveries(memory_id, delivered_at);
        CREATE TABLE memory_evidence (
            memory_id INTEGER NOT NULL, source_session TEXT, turn_id TEXT NOT NULL,
            role TEXT NOT NULL, excerpt TEXT NOT NULL
        );
        CREATE TABLE relevance_judgments (
            id INTEGER PRIMARY KEY, memory_id INTEGER NOT NULL, session_id TEXT NOT NULL,
            prompt TEXT NOT NULL, model TEXT NOT NULL, logit REAL NOT NULL, judged_at TEXT NOT NULL
        );

        INSERT INTO memories (id, kind, entity, title, body, created, updated, pinned, archived, class, archive_reason)
        VALUES (7, 'observation', NULL, 'Prefers rebase', 'Rebase feature branches.', '2026-09-01T00:00:00Z',
                '2026-09-02T00:00:00Z', 1, 0, 'preference', NULL),
               (12, 'card', 'person:jason', 'Jason', 'Works on the iOS app.', '2026-09-03T00:00:00Z',
                '2026-09-03T00:00:00Z', 0, 1, NULL, 'manual');
        INSERT INTO links VALUES (7, 'Jason');
        INSERT INTO embeddings VALUES (7, 'potion-base-8M', x'0000803f');
        INSERT INTO memory_deliveries (memory_id, session_id, event, form, delivered_at)
        VALUES (7, 's1', 'prompt', 'full', '2026-09-04T00:00:00Z'), (12, 's1', 'prompt', 'pointer', '2026-09-04T00:00:00Z');
        INSERT INTO memory_evidence VALUES (7, 's1', 'N1', 'user', 'always rebase');
        INSERT INTO relevance_judgments (memory_id, session_id, prompt, model, logit, judged_at)
        VALUES (12, 's1', 'who does ios', 'ms-marco-MiniLM-L6-v2', 3.5, '2026-09-04T00:00:00Z');

        PRAGMA user_version = 6;
    ";

    #[test]
    fn counted_ids_migrate_to_global_ones_with_everything_still_attached() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(V6_STORE).unwrap();
        let memory = Memory { connection, directory: None };
        memory.migrate().unwrap();

        let ids = memory.ids().unwrap();
        assert_eq!(ids.len(), 2);
        let rebase = memory.portable(ids[0]).unwrap();
        assert_eq!(rebase.title, "Prefers rebase");
        assert_eq!(rebase.links, vec!["Jason"]);
        assert_eq!(rebase.class.as_deref(), Some("preference"));
        assert!(rebase.pinned);
        assert_eq!(rebase.created, "2026-09-01T00:00:00Z");

        let jason = memory.portable(ids[1]).unwrap();
        assert!(jason.archived);
        assert_eq!(jason.archive_reason.as_deref(), Some("manual"));

        let attached = |table: &str, column: &str, id: Id| -> i64 {
            memory
                .connection
                .query_row(&format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"), [id], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(attached("embeddings", "memory_id", ids[0]), 1);
        assert_eq!(attached("memory_deliveries", "memory_id", ids[0]), 1);
        assert_eq!(attached("memory_deliveries", "memory_id", ids[1]), 1);
        assert_eq!(attached("memory_evidence", "memory_id", ids[0]), 1);
        assert_eq!(attached("relevance_judgments", "memory_id", ids[1]), 1);

        let hits = crate::search::bm25(&memory, "rebase feature branches", 5).unwrap();
        assert_eq!(hits.iter().map(|it| it.id).collect::<Vec<_>>(), vec![ids[0]]);

        let added = observation_about(&memory, "person:jason", "Learned after the migration");
        assert_eq!(memory.ids().unwrap(), vec![ids[0], ids[1], added]);
    }

    #[test]
    fn a_typed_id_can_be_any_prefix_that_singles_one_memory_out() {
        let memory = Memory::open_in_memory().unwrap();
        let portable = |id: &str, title: &str| Portable {
            id: Id::parse(id).unwrap(),
            kind: Kind::Observation,
            class: None,
            entity: None,
            title: title.into(),
            body: "Body.".into(),
            links: vec![],
            pinned: false,
            archived: false,
            archive_reason: None,
            created: "2026-09-01T00:00:00Z".into(),
            updated: "2026-09-01T00:00:00Z".into(),
        };
        let first = memory.import(&portable("k7m2p9xq", "First")).unwrap();
        let second = memory.import(&portable("k7zz0000", "Second")).unwrap();

        assert_eq!(memory.resolve("k7m2p9xq").unwrap(), first);
        assert_eq!(memory.resolve("k7m").unwrap(), first);
        assert_eq!(memory.resolve("k7z").unwrap(), second);
        assert!(memory.resolve("k7").unwrap_err().to_string().contains("several ids"));
        assert!(memory.resolve("b").unwrap_err().to_string().contains("no memory with id"));
        assert!(memory.resolve("k%").is_err());
        assert!(memory.resolve("").is_err());
    }

    fn observation_about(memory: &Memory, entity: &str, title: &str) -> Id {
        memory
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: Some(entity.into()),
                title: title.into(),
                body: "Body.".into(),
                links: vec![],
                source_session: None,
                class: None,
            })
            .unwrap()
    }

    fn checkout(entity: &str, paths: &[&str], root_commit: &str) -> Project {
        Project {
            entity: entity.into(),
            aliases: paths.iter().map(|it| format!("project:{it}")).collect(),
            root_commit: Some(root_commit.into()),
        }
    }

    #[test]
    fn settling_a_project_moves_its_path_named_memories_home() {
        let memory = Memory::open_in_memory().unwrap();
        let from_root = observation_about(&memory, "project:/home/someone/app", "From the root");
        let from_worktree = observation_about(&memory, "project:/home/someone/app-wt", "From a worktree");
        let unrelated = observation_about(&memory, "project:/home/someone/other", "Unrelated");

        let app = "project:example.com/acme/app";
        memory
            .settle_project(&checkout(app, &["/home/someone/app-wt", "/home/someone/app"], "aaa"))
            .unwrap();

        assert_eq!(memory.get(from_root).unwrap().entity.as_deref(), Some(app));
        assert_eq!(memory.get(from_worktree).unwrap().entity.as_deref(), Some(app));
        assert_eq!(
            memory.get(unrelated).unwrap().entity.as_deref(),
            Some("project:/home/someone/other")
        );
    }

    #[test]
    fn a_renamed_remote_takes_its_memories_along_but_a_reused_directory_does_not() {
        let memory = Memory::open_in_memory().unwrap();
        let old_name = "project:example.com/acme/app";
        memory.settle_project(&checkout(old_name, &["/home/someone/app"], "aaa")).unwrap();
        let learned = observation_about(&memory, old_name, "Learned before the rename");

        let new_name = "project:example.com/acme/application";
        memory.settle_project(&checkout(new_name, &["/home/someone/app"], "aaa")).unwrap();
        assert_eq!(memory.get(learned).unwrap().entity.as_deref(), Some(new_name));

        let elsewhere = observation_about(&memory, old_name, "Synced in under the old name");
        memory.settle_project(&checkout(new_name, &["/home/someone/app"], "aaa")).unwrap();
        assert_eq!(memory.get(elsewhere).unwrap().entity.as_deref(), Some(new_name));

        let different = "project:example.com/acme/something-else";
        memory.settle_project(&checkout(different, &["/home/someone/app"], "bbb")).unwrap();
        assert_eq!(memory.get(learned).unwrap().entity.as_deref(), Some(new_name));

        memory.settle_project(&checkout(old_name, &["/home/someone/app"], "bbb")).unwrap();
        assert_eq!(memory.get(learned).unwrap().entity.as_deref(), Some(new_name));
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
