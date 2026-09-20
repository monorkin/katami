//! The little the mesh has to agree on besides the memories themselves.
//!
//! Two things. Who curates: left alone, every machine would fold the same
//! observations into the same card every day and they'd spend their time
//! merging each other's rewrites, so one of them holds a lease and the rest
//! stand by — a coordinator, but one that moves to whoever is awake when the
//! lease lapses, so no machine has to be up for the others to be looked
//! after. And what gets used: a memory is retired for never being retrieved,
//! and retrieval happens on whichever machine a project is worked on, so
//! each machine marks the day it last delivered a memory and the marks are
//! pooled — unretrieved has to mean unretrieved anywhere.
//!
//! Neither needs version vectors. The lease is one value where the latest
//! write winning is the whole point, and a usage mark only ever moves
//! forward, so pooling two of them is taking the later day.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::clock::timestamp;
use crate::id::Id;
use crate::memory::Memory;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SharedValue {
    pub key: String,
    pub value: String,
    pub updated: String,
    pub node: Id,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UsageMark {
    pub memory_id: Id,
    pub node: Id,
    pub last_delivered: String,
}

impl Memory {
    pub fn shared(&self, key: &str) -> Result<Option<String>> {
        let mut statement = self.connection.prepare("SELECT value FROM shared_state WHERE key = ?1")?;
        let mut rows = statement.query_map([key], |row| row.get(0))?;
        Ok(rows.next().transpose()?)
    }

    pub fn share(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO shared_state (key, value, updated, node) VALUES (?1, ?2, ?3, (SELECT node FROM sync_clock))
             ON CONFLICT (key) DO UPDATE SET value = ?2, updated = ?3, node = excluded.node",
            rusqlite::params![key, value, timestamp()],
        )?;
        Ok(())
    }

    pub fn shared_values(&self) -> Result<Vec<SharedValue>> {
        let mut statement = self
            .connection
            .prepare("SELECT key, value, updated, node FROM shared_state ORDER BY key")?;
        let rows = statement.query_map([], |row| {
            Ok(SharedValue {
                key: row.get(0)?,
                value: row.get(1)?,
                updated: row.get(2)?,
                node: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The later write wins, and the node breaks a tie the same way on every
    /// machine.
    pub fn hear_shared(&self, values: &[SharedValue]) -> Result<()> {
        for value in values {
            self.connection.execute(
                "INSERT INTO shared_state (key, value, updated, node) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (key) DO UPDATE SET value = ?2, updated = ?3, node = ?4
                 WHERE ?3 > updated OR (?3 = updated AND ?4 > node)",
                rusqlite::params![value.key, value.value, value.updated, value.node],
            )?;
        }
        Ok(())
    }

    /// One mark per memory per day at most, so a memory injected into every
    /// prompt of a busy session costs the mesh one row.
    pub fn mark_used(&self, memory_id: Id) -> Result<()> {
        let now = timestamp();
        self.connection.execute(
            "INSERT INTO usage_marks (memory_id, node, last_delivered, learned)
             VALUES (?1, (SELECT node FROM sync_clock), ?2, ?3)
             ON CONFLICT (memory_id, node) DO UPDATE SET last_delivered = ?2, learned = ?3
             WHERE ?2 > last_delivered",
            rusqlite::params![memory_id, &now[..10], now],
        )?;
        Ok(())
    }

    /// Marks this store learned of since `since` — its own and relayed ones
    /// alike, which is what lets them travel through a third machine.
    pub fn usage_marks_since(&self, since: Option<&str>) -> Result<Vec<UsageMark>> {
        let mut statement = self.connection.prepare(
            "SELECT memory_id, node, last_delivered FROM usage_marks
             WHERE ?1 IS NULL OR learned >= ?1 ORDER BY memory_id, node",
        )?;
        let rows = statement.query_map([since], |row| {
            Ok(UsageMark {
                memory_id: row.get(0)?,
                node: row.get(1)?,
                last_delivered: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn hear_usage(&self, marks: &[UsageMark]) -> Result<()> {
        let now = timestamp();
        for mark in marks {
            self.connection.execute(
                "INSERT INTO usage_marks (memory_id, node, last_delivered, learned) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (memory_id, node) DO UPDATE SET last_delivered = ?3, learned = ?4
                 WHERE ?3 > last_delivered",
                rusqlite::params![mark.memory_id, mark.node, mark.last_delivered, now],
            )?;
        }
        Ok(())
    }

    pub fn marks_sent_to(&self, node: Id) -> Result<Option<String>> {
        let mut statement = self.connection.prepare("SELECT marks_sent FROM peers WHERE node = ?1")?;
        let mut rows = statement.query_map([node], |row| row.get::<_, Option<String>>(0))?;
        Ok(rows.next().transpose()?.flatten())
    }

    pub fn set_marks_sent_to(&self, node: Id, when: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE peers SET marks_sent = ?2 WHERE node = ?1",
            rusqlite::params![node, when],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(key: &str, value: &str, updated: &str, node: &str) -> SharedValue {
        SharedValue {
            key: key.into(),
            value: value.into(),
            updated: updated.into(),
            node: Id::parse(node).unwrap(),
        }
    }

    #[test]
    fn the_latest_shared_value_wins_the_same_way_everywhere() {
        let memory = Memory::open_in_memory().unwrap();
        memory.hear_shared(&[value("lease", "mini", "2026-09-10T00:00:00Z", "mmmmmmmm")]).unwrap();
        memory.hear_shared(&[value("lease", "stale", "2026-09-09T00:00:00Z", "zzzzzzzz")]).unwrap();
        assert_eq!(memory.shared("lease").unwrap().as_deref(), Some("mini"));

        memory.hear_shared(&[value("lease", "tie goes to the higher node", "2026-09-10T00:00:00Z", "pppppppp")]).unwrap();
        assert_eq!(memory.shared("lease").unwrap().as_deref(), Some("tie goes to the higher node"));
        memory.hear_shared(&[value("lease", "mini", "2026-09-10T00:00:00Z", "mmmmmmmm")]).unwrap();
        assert_eq!(memory.shared("lease").unwrap().as_deref(), Some("tie goes to the higher node"));

        memory.share("lease", "mine now").unwrap();
        assert_eq!(memory.shared("lease").unwrap().as_deref(), Some("mine now"));
        assert_eq!(memory.shared_values().unwrap()[0].node, memory.node().unwrap());
        assert_eq!(memory.shared("nothing").unwrap(), None);
    }

    #[test]
    fn usage_marks_only_move_forward_and_travel_through_a_third_machine() {
        let (mini, desktop, laptop) = (
            Memory::open_in_memory().unwrap(),
            Memory::open_in_memory().unwrap(),
            Memory::open_in_memory().unwrap(),
        );
        let id = Id::parse("k7m2p9xq").unwrap();

        mini.mark_used(id).unwrap();
        mini.mark_used(id).unwrap();
        let from_mini = mini.usage_marks_since(None).unwrap();
        assert_eq!(from_mini.len(), 1);
        assert_eq!(from_mini[0].node, mini.node().unwrap());

        desktop.hear_usage(&from_mini).unwrap();
        laptop.hear_usage(&desktop.usage_marks_since(None).unwrap()).unwrap();
        assert_eq!(laptop.usage_marks_since(None).unwrap(), from_mini);

        let older = UsageMark { last_delivered: "2020-01-01".into(), ..from_mini[0].clone() };
        laptop.hear_usage(&[older]).unwrap();
        assert_eq!(laptop.usage_marks_since(None).unwrap(), from_mini);

        assert!(laptop.usage_marks_since(Some("2999-01-01T00:00:00Z")).unwrap().is_empty());
    }
}
