//! One store among several: what it knows, what it can tell a peer, and what
//! it does with what a peer tells it.
//!
//! There's no server. Every store holds everything, and any two can sync:
//! one says how far it has caught up with each node it has ever heard of, the
//! other answers with every version past that — its own writes and ones it
//! picked up elsewhere — so memories reach a machine through whichever peer
//! it happens to meet, and a machine that's off holds nobody up.
//!
//! An arriving version either is new, grew out of what's here (take it), is
//! already covered (ignore it), or was written at the same time as the local
//! one. That last case is never settled by picking a winner, with two
//! exceptions that lose nothing: both sides say the same thing, or both are
//! merges of the same two versions. Otherwise the arrival is kept beside the
//! local version as a sibling until the two are merged, and it's passed on to
//! peers meanwhile so it can't be lost with this machine.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::clock::timestamp;
use crate::id::Id;
use crate::memory::{Memory, Portable};
use crate::version::{Relation, Version};

/// One write: the node that made it and the tick of that node's clock.
/// Ordered by tick, then node, which is only ever used to pick the same
/// side of a tie on every machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Dot {
    pub seq: u64,
    pub node: Id,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub memory: Portable,
    pub dot: Dot,
    pub version: Version,
    pub merged_from: Option<String>,
}

/// How far a store has caught up with each node: every write from `node` up
/// to the tick is either here or superseded by something that is.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Knowledge(BTreeMap<Id, u64>);

impl Knowledge {
    pub fn covers(&self, dot: &Dot) -> bool {
        self.caught_up_with(&dot.node) >= dot.seq
    }

    fn caught_up_with(&self, node: &Id) -> u64 {
        self.0.get(node).copied().unwrap_or(0)
    }
}

/// The logs katami keeps about its memories: when each was delivered, how
/// the reranker judged it, which turns of a conversation it came from, and
/// when a skill got used. A row is written once and never changed, so it
/// shares by simply turning up — no versions, nothing to conflict. The
/// column lists are all there is to know about a log, so they're data.
struct Log {
    table: &'static str,
    columns: &'static [&'static str],
}

const LOGS: [Log; 4] = [
    Log {
        table: "memory_deliveries",
        columns: &["memory_id", "session_id", "event", "form", "delivered_at"],
    },
    Log {
        table: "relevance_judgments",
        columns: &["memory_id", "session_id", "prompt", "model", "logit", "judged_at"],
    },
    Log {
        table: "memory_evidence",
        columns: &["memory_id", "source_session", "turn_id", "role", "excerpt"],
    },
    Log {
        table: "usage",
        columns: &["kind", "name", "session_id", "used_at"],
    },
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogRows {
    pub table: String,
    pub rows: Vec<LogRow>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogRow {
    pub dot: Dot,
    pub values: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    pub node: Id,
    pub knowledge: Knowledge,
    pub records: Vec<Record>,
    pub logs: Vec<LogRows>,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Absorbed {
    New,
    Updated,
    Settled,
    Conflicted,
    Ignored,
}

#[derive(Debug, Default, PartialEq)]
pub struct Tally {
    pub changed: Vec<Id>,
    pub conflicted: Vec<Id>,
}

impl Memory {
    pub fn node(&self) -> Result<Id> {
        Ok(self.connection.query_row("SELECT node FROM sync_clock", [], |row| row.get(0))?)
    }

    pub fn knowledge(&self) -> Result<Knowledge> {
        let mut statement = self.connection.prepare(
            "SELECT node, seq FROM sync_knowledge WHERE node != (SELECT node FROM sync_clock)
             UNION ALL SELECT node, seq FROM sync_clock",
        )?;
        let known = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<Id, u64>>>()?;
        Ok(Knowledge(known))
    }

    /// Everything here that `theirs` hasn't caught up with — siblings
    /// included, since an unmerged conflict must survive this machine.
    pub fn delta_for(&self, theirs: &Knowledge) -> Result<Delta> {
        let mut statement = self.connection.prepare("SELECT id, dot_node, dot_seq FROM memories ORDER BY local_row")?;
        let stamped = statement
            .query_map([], |row| Ok((row.get(0)?, Dot { node: row.get(1)?, seq: row.get(2)? })))?
            .collect::<rusqlite::Result<Vec<(Id, Dot)>>>()?;

        let mut records = Vec::new();
        for (id, dot) in stamped {
            if !theirs.covers(&dot) {
                records.push(self.record(id)?);
            }
        }
        for sibling in self.all_siblings()? {
            if !theirs.covers(&sibling.dot) {
                records.push(sibling);
            }
        }

        Ok(Delta {
            node: self.node()?,
            knowledge: self.knowledge()?,
            records,
            logs: LOGS
                .iter()
                .map(|it| self.log_rows_beyond(it, theirs))
                .collect::<Result<Vec<_>>>()?,
        })
    }

    /// A delta lands whole: its knowledge only holds once every record in it
    /// has been dealt with.
    pub fn absorb_delta(&self, delta: &Delta) -> Result<Tally> {
        self.with_transaction(|memory| {
            let mut tally = Tally::default();
            for record in &delta.records {
                match memory.absorb(record)? {
                    Absorbed::New | Absorbed::Updated | Absorbed::Settled => tally.changed.push(record.memory.id),
                    Absorbed::Conflicted => tally.conflicted.push(record.memory.id),
                    Absorbed::Ignored => {}
                }
            }
            for rows in &delta.logs {
                memory.absorb_log_rows(rows)?;
            }
            memory.learn(&delta.knowledge)?;
            Ok(tally)
        })
    }

    pub fn absorb(&self, arriving: &Record) -> Result<Absorbed> {
        if !self.exists(arriving.memory.id)? {
            self.write_record(arriving)?;
            return Ok(Absorbed::New);
        }

        let local = self.record(arriving.memory.id)?;
        match arriving.version.relation_to(&local.version) {
            Relation::Same | Relation::Older => Ok(Absorbed::Ignored),
            Relation::Newer => {
                self.write_record(arriving)?;
                self.drop_siblings_covered_by(arriving)?;
                Ok(Absorbed::Updated)
            }
            Relation::Concurrent => self.absorb_concurrent(&local, arriving),
        }
    }

    pub fn record(&self, id: Id) -> Result<Record> {
        let memory = self.portable(id)?;
        let (dot, version, merged_from) = self.connection.query_row(
            "SELECT dot_node, dot_seq, version, merged_from FROM memories WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    Dot { node: row.get(0)?, seq: row.get(1)? },
                    row.get::<_, String>(2)?,
                    row.get(3)?,
                ))
            },
        )?;
        Ok(Record { memory, dot, version: Version::parse(&version)?, merged_from })
    }

    pub fn siblings_of(&self, id: Id) -> Result<Vec<Record>> {
        let mut statement = self
            .connection
            .prepare("SELECT record FROM memory_siblings WHERE memory_id = ?1 ORDER BY dot_seq, dot_node")?;
        let records = statement
            .query_map([id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        records.iter().map(|it| Ok(serde_json::from_str(it)?)).collect()
    }

    pub fn conflicted_ids(&self) -> Result<Vec<Id>> {
        let mut statement = self
            .connection
            .prepare("SELECT DISTINCT memory_id FROM memory_siblings ORDER BY memory_id")?;
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The outcome of merging the local version with `sibling`: the merged
    /// memory descends from both, and the stamping trigger makes it this
    /// node's write, so it reaches every peer as a plain newer version.
    pub fn resolve_sibling(&self, merged: &Portable, sibling: &Record) -> Result<()> {
        self.with_transaction(|memory| {
            let local = memory.record(merged.id)?;
            let parents = merge_name(&local.dot, &sibling.dot);
            memory.overwrite(merged.id, merged)?;
            memory.connection.execute(
                "UPDATE memories SET version = ?2, merged_from = ?3 WHERE id = ?1",
                rusqlite::params![merged.id, local.version.union(&sibling.version).to_json(), parents],
            )?;
            memory.drop_sibling(merged.id, &sibling.dot)
        })
    }

    /// The rows of one log that `theirs` hasn't caught up with, asked for
    /// node by node so the index on the dot does the work and the whole log
    /// is never read.
    fn log_rows_beyond(&self, log: &Log, theirs: &Knowledge) -> Result<LogRows> {
        let mut statement = self
            .connection
            .prepare(&format!("SELECT DISTINCT dot_node FROM {} WHERE dot_node IS NOT NULL", log.table))?;
        let nodes = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<Id>>>()?;

        let mut rows = Vec::new();
        for node in nodes {
            let mut statement = self.connection.prepare(&format!(
                "SELECT dot_seq, {} FROM {} WHERE dot_node = ?1 AND dot_seq > ?2 ORDER BY dot_seq",
                log.columns.join(", "),
                log.table
            ))?;
            let beyond = statement.query_map(rusqlite::params![node, theirs.caught_up_with(&node)], |row| {
                let values = (1..=log.columns.len())
                    .map(|column| row.get_ref(column).map(json_of))
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(LogRow { dot: Dot { node, seq: row.get(0)? }, values })
            })?;
            rows.extend(beyond.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(LogRows { table: log.table.to_string(), rows })
    }

    /// A row that's already here is recognized by its dot and skipped, which
    /// is all the conflict handling a write-once log needs. The table name
    /// came off the wire, so it's only ever used to pick one of ours.
    fn absorb_log_rows(&self, arriving: &LogRows) -> Result<()> {
        let log = LOGS
            .iter()
            .find(|it| it.table == arriving.table)
            .with_context(|| format!("a peer sent rows for `{}`, which isn't a log this katami shares — upgrade with `katami upgrade`", arriving.table))?;

        let placeholders: Vec<String> = (1..=log.columns.len() + 2).map(|it| format!("?{it}")).collect();
        let mut statement = self.connection.prepare(&format!(
            "INSERT OR IGNORE INTO {} ({}, dot_node, dot_seq) VALUES ({})",
            log.table,
            log.columns.join(", "),
            placeholders.join(", ")
        ))?;
        for row in &arriving.rows {
            if row.values.len() != log.columns.len() {
                bail!("a peer sent a malformed `{}` row", log.table);
            }
            let mut values: Vec<rusqlite::types::Value> = row.values.iter().map(sql_of).collect();
            values.push(rusqlite::types::Value::Text(row.dot.node.to_string()));
            values.push(rusqlite::types::Value::Integer(row.dot.seq as i64));
            statement.execute(rusqlite::params_from_iter(values))?;
        }
        Ok(())
    }

    fn absorb_concurrent(&self, local: &Record, arriving: &Record) -> Result<Absorbed> {
        let same_merge = local.merged_from.is_some() && local.merged_from == arriving.merged_from;
        if local.memory.says_the_same_as(&arriving.memory) || same_merge {
            // Settling is this node's write, stamped by the triggers: a peer
            // that already knows both dots would otherwise never hear that
            // they've been reconciled
            let winner = if arriving.dot > local.dot { arriving } else { local };
            self.overwrite(winner.memory.id, &winner.memory)?;
            self.connection.execute(
                "UPDATE memories SET version = ?2 WHERE id = ?1",
                rusqlite::params![winner.memory.id, local.version.union(&arriving.version).to_json()],
            )?;
            self.drop_siblings_covered_by(&self.record(winner.memory.id)?)?;
            Ok(Absorbed::Settled)
        } else {
            self.connection.execute(
                "INSERT OR IGNORE INTO memory_siblings (memory_id, dot_node, dot_seq, record, received)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    arriving.memory.id,
                    arriving.dot.node,
                    arriving.dot.seq,
                    serde_json::to_string(arriving)?,
                    timestamp()
                ],
            )?;
            Ok(Absorbed::Conflicted)
        }
    }

    /// Writes a version exactly as it was stamped elsewhere. Writing the
    /// content and links trips the stamping triggers, which would claim the
    /// write for this node; the stamp goes on last and in two steps, because
    /// the triggers stand down only for an update that changes the dot, and
    /// the dot being restored can be the one already there.
    fn write_record(&self, record: &Record) -> Result<()> {
        if self.exists(record.memory.id)? {
            self.overwrite(record.memory.id, &record.memory)?;
        } else {
            self.import(&record.memory)?;
        }
        self.connection.execute(
            "UPDATE memories SET dot_seq = -1 WHERE id = ?1",
            [record.memory.id],
        )?;
        self.connection.execute(
            "UPDATE memories SET dot_node = ?2, dot_seq = ?3, version = ?4, merged_from = ?5 WHERE id = ?1",
            rusqlite::params![
                record.memory.id,
                record.dot.node,
                record.dot.seq,
                record.version.to_json(),
                record.merged_from
            ],
        )?;
        Ok(())
    }

    fn learn(&self, theirs: &Knowledge) -> Result<()> {
        for (node, seq) in &theirs.0 {
            self.connection.execute(
                "INSERT INTO sync_knowledge (node, seq) VALUES (?1, ?2)
                 ON CONFLICT (node) DO UPDATE SET seq = MAX(seq, ?2)",
                rusqlite::params![node, seq],
            )?;
        }
        Ok(())
    }

    fn all_siblings(&self) -> Result<Vec<Record>> {
        let mut statement = self
            .connection
            .prepare("SELECT record FROM memory_siblings ORDER BY memory_id, dot_seq, dot_node")?;
        let records = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        records.iter().map(|it| Ok(serde_json::from_str(it)?)).collect()
    }

    fn drop_siblings_covered_by(&self, record: &Record) -> Result<()> {
        for sibling in self.siblings_of(record.memory.id)? {
            if matches!(sibling.version.relation_to(&record.version), Relation::Same | Relation::Older) {
                self.drop_sibling(record.memory.id, &sibling.dot)?;
            }
        }
        Ok(())
    }

    fn drop_sibling(&self, id: Id, dot: &Dot) -> Result<()> {
        self.connection.execute(
            "DELETE FROM memory_siblings WHERE memory_id = ?1 AND dot_node = ?2 AND dot_seq = ?3",
            rusqlite::params![id, dot.node, dot.seq],
        )?;
        Ok(())
    }
}

fn json_of(value: rusqlite::types::ValueRef) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(integer) => integer.into(),
        ValueRef::Real(real) => real.into(),
        ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned().into(),
        ValueRef::Blob(_) => serde_json::Value::Null,
    }
}

fn sql_of(value: &serde_json::Value) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match value {
        serde_json::Value::String(text) => Value::Text(text.clone()),
        serde_json::Value::Number(number) => match number.as_i64() {
            Some(integer) => Value::Integer(integer),
            None => Value::Real(number.as_f64().unwrap_or_default()),
        },
        serde_json::Value::Bool(flag) => Value::Integer(*flag as i64),
        _ => Value::Null,
    }
}

/// Names a merge by its two parents, whichever order they were met in, so two
/// machines that merge the same pair can tell that they did.
fn merge_name(one: &Dot, other: &Dot) -> String {
    let (first, second) = if one < other { (one, other) } else { (other, one) };
    format!("{}:{}+{}:{}", first.node, first.seq, second.node, second.seq)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Kind, NewMemory};

    fn store() -> Memory {
        Memory::open_in_memory().unwrap()
    }

    fn learn(memory: &Memory, title: &str, body: &str) -> Id {
        memory
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: Some("project:example.com/acme/app".into()),
                title: title.into(),
                body: body.into(),
                links: vec!["Deploys".into()],
                source_session: None,
                class: Some("decision".into()),
            })
            .unwrap()
    }

    fn pull(into: &Memory, from: &Memory) -> Tally {
        let delta = from.delta_for(&into.knowledge().unwrap()).unwrap();
        into.absorb_delta(&delta).unwrap()
    }

    fn body(memory: &Memory, id: Id) -> String {
        memory.portable(id).unwrap().body
    }

    #[test]
    fn memories_reach_a_machine_through_whichever_peer_it_meets() {
        let (mini, desktop, laptop) = (store(), store(), store());
        let id = learn(&mini, "Deploy on Fridays", "We deploy on Fridays.");

        assert_eq!(pull(&desktop, &mini).changed, vec![id]);
        assert_eq!(pull(&laptop, &desktop).changed, vec![id]);

        assert_eq!(laptop.portable(id).unwrap(), mini.portable(id).unwrap());
        assert_eq!(laptop.record(id).unwrap(), mini.record(id).unwrap());

        assert_eq!(pull(&laptop, &mini), Tally::default());
        assert!(mini.delta_for(&laptop.knowledge().unwrap()).unwrap().records.is_empty());
    }

    #[test]
    fn every_kind_of_edit_follows_the_memory() {
        let (mini, laptop) = (store(), store());
        let id = learn(&mini, "Deploy on Fridays", "We deploy on Fridays.");
        pull(&laptop, &mini);

        mini.update_body(id, "We deploy on Thursdays now.").unwrap();
        assert_eq!(pull(&laptop, &mini).changed, vec![id]);
        assert_eq!(body(&laptop, id), "We deploy on Thursdays now.");

        mini.replace_links(id, &["Release checklist".to_string()]).unwrap();
        pull(&laptop, &mini);
        assert_eq!(laptop.portable(id).unwrap().links, vec!["Release checklist"]);

        laptop.archive(id, "manual").unwrap();
        assert_eq!(pull(&mini, &laptop).changed, vec![id]);
        assert!(mini.portable(id).unwrap().archived);
        assert_eq!(pull(&laptop, &mini), Tally::default());
    }

    #[test]
    fn two_machines_editing_at_once_lose_nothing() {
        let (mini, desktop, laptop) = (store(), store(), store());
        let id = learn(&mini, "Deploy on Fridays", "We deploy on Fridays.");
        pull(&desktop, &mini);

        mini.update_body(id, "We deploy on Fridays, after standup.").unwrap();
        desktop.update_body(id, "We deploy on Fridays, never before a holiday.").unwrap();

        assert_eq!(pull(&desktop, &mini).conflicted, vec![id]);
        assert_eq!(body(&desktop, id), "We deploy on Fridays, never before a holiday.");
        assert_eq!(desktop.conflicted_ids().unwrap(), vec![id]);

        pull(&laptop, &desktop);
        assert_eq!(laptop.siblings_of(id).unwrap()[0].memory.body, "We deploy on Fridays, after standup.");

        let sibling = desktop.siblings_of(id).unwrap().remove(0);
        let merged = Portable {
            body: "We deploy on Fridays after standup, never before a holiday.".into(),
            ..desktop.portable(id).unwrap()
        };
        desktop.resolve_sibling(&merged, &sibling).unwrap();
        assert!(desktop.conflicted_ids().unwrap().is_empty());

        for machine in [&mini, &laptop] {
            assert_eq!(pull(machine, &desktop).changed, vec![id]);
            assert_eq!(body(machine, id), "We deploy on Fridays after standup, never before a holiday.");
            assert!(machine.conflicted_ids().unwrap().is_empty());
        }
    }

    fn count(memory: &Memory, table: &str) -> i64 {
        memory
            .connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn the_logs_travel_with_the_memories_and_never_double_up() {
        let (mini, desktop, laptop) = (store(), store(), store());
        let id = learn(&laptop, "Deploy on Fridays", "We deploy on Fridays.");
        laptop.record_delivery(id, "session-1", "prompt", "full").unwrap();
        laptop.record_judgment(id, "session-1", "when do we deploy?", "ms-marco-MiniLM-L6-v2", 3.0).unwrap();
        laptop.add_evidence(id, Some("session-1"), "N1", "user", "we deploy on fridays").unwrap();
        laptop.record_usage("skill", "katami-deploy-check", "session-1").unwrap();

        pull(&desktop, &laptop);
        pull(&mini, &desktop);
        pull(&mini, &laptop);
        pull(&laptop, &mini);

        for machine in [&mini, &desktop, &laptop] {
            for log in ["memory_deliveries", "relevance_judgments", "memory_evidence", "usage"] {
                assert_eq!(count(machine, log), 1, "{log}");
            }
        }
        let logit: f64 = mini
            .connection
            .query_row("SELECT logit FROM relevance_judgments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(logit, 3.0);
        assert_eq!(mini.last_used("skill", "katami-deploy-check").unwrap(), laptop.last_used("skill", "katami-deploy-check").unwrap());
    }

    #[test]
    fn a_memory_delivered_on_any_machine_is_not_retirable_on_another() {
        let (mini, laptop) = (store(), store());
        let used_on_the_laptop = learn(&mini, "Deploy on Fridays", "We deploy on Fridays.");
        let used_nowhere = learn(&mini, "Lunch is at noon", "Lunch is at noon.");
        mini.connection.execute("UPDATE memories SET class = 'history'", []).unwrap();
        pull(&laptop, &mini);

        laptop.record_delivery(used_on_the_laptop, "session-1", "prompt", "full").unwrap();
        pull(&mini, &laptop);

        let retirable: Vec<Id> = mini.unretrieved_observations().unwrap().iter().map(|it| it.id).collect();
        assert_eq!(retirable, vec![used_nowhere]);
    }

    #[test]
    fn rows_for_a_log_this_katami_does_not_share_are_refused() {
        let memory = store();
        let mut delta = store().delta_for(&memory.knowledge().unwrap()).unwrap();
        delta.logs.push(LogRows { table: "peers".into(), rows: vec![] });
        assert!(memory.absorb_delta(&delta).is_err());
    }

    #[test]
    fn the_same_edit_made_twice_is_not_a_conflict() {
        let (mini, laptop) = (store(), store());
        let id = learn(&mini, "Deploy on Fridays", "We deploy on Fridays.");
        pull(&laptop, &mini);

        mini.archive(id, "manual").unwrap();
        laptop.archive(id, "manual").unwrap();

        assert_eq!(pull(&laptop, &mini).changed, vec![id]);
        assert!(laptop.conflicted_ids().unwrap().is_empty());
        pull(&mini, &laptop);
        assert_eq!(mini.record(id).unwrap().version, laptop.record(id).unwrap().version);
        assert_eq!(pull(&laptop, &mini), Tally::default());
    }

    #[test]
    fn two_machines_merging_the_same_conflict_settle_on_one_merge() {
        let (mini, laptop) = (store(), store());
        let id = learn(&mini, "Deploy on Fridays", "We deploy on Fridays.");
        pull(&laptop, &mini);
        mini.update_body(id, "Fridays, after standup.").unwrap();
        laptop.update_body(id, "Fridays, never before a holiday.").unwrap();
        pull(&laptop, &mini);
        pull(&mini, &laptop);

        for (machine, wording) in [(&mini, "Merged on the mini."), (&laptop, "Merged on the laptop.")] {
            let sibling = machine.siblings_of(id).unwrap().remove(0);
            let merged = Portable { body: wording.into(), ..machine.portable(id).unwrap() };
            machine.resolve_sibling(&merged, &sibling).unwrap();
        }

        pull(&laptop, &mini);
        pull(&mini, &laptop);
        assert_eq!(body(&mini, id), body(&laptop, id));
        assert!(mini.conflicted_ids().unwrap().is_empty() && laptop.conflicted_ids().unwrap().is_empty());
        assert_eq!(pull(&laptop, &mini), Tally::default());
    }
}