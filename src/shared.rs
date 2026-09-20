//! The little the mesh has to agree on besides what it remembers.
//!
//! So far that's one thing: who curates. Left alone, every machine would fold
//! the same observations into the same card every day and they'd spend their
//! time merging each other's rewrites, so one of them holds a lease and the
//! rest stand by — a coordinator, but one that moves to whoever is awake when
//! the lease lapses, so no machine has to be up for the others to be looked
//! after.
//!
//! A shared value needs no version vector. It's a single cell where the
//! latest write winning is the whole point, and nothing a person said is
//! lost when it's overwritten.

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
}
