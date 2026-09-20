//! The other machines this store shares memory with.
//!
//! The list is shared like the memories are: every sync swaps peer cards, so
//! linking a laptop to any one machine introduces it to all of them, and
//! nobody has to be the hub. A card is public — a node, a name, an address —
//! and whether a newcomer is actually let in is decided by each machine for
//! itself, by Tailscale ownership or by pairing. The token a pairing produces
//! is a secret between two machines and never rides on a card.
//!
//! Peer cards are settled by whichever was updated last. That's fine here,
//! where memories needed version vectors: a stale address costs one failed
//! connection, not something a person said. Removing a peer keeps its card,
//! marked removed, or the next sync would hear of it again and bring it back.

use anyhow::{Context, Result, bail};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::clock::timestamp;
use crate::id::Id;
use crate::memory::Memory;

#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub node: Id,
    pub name: String,
    pub address: String,
    pub token: Option<String>,
    pub last_synced: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerCard {
    pub node: Id,
    pub name: String,
    pub address: String,
    pub removed: bool,
    pub updated: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pairing {
    pub code: String,
    pub node: Id,
    pub name: String,
    pub address: String,
    pub token: Option<String>,
}

impl Memory {
    pub fn peers(&self) -> Result<Vec<Peer>> {
        let mut statement = self.connection.prepare(
            "SELECT node, name, address, token, last_synced FROM peers WHERE removed = 0 ORDER BY name, node",
        )?;
        let rows = statement.query_map([], row_to_peer)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn peer(&self, node: Id) -> Result<Option<Peer>> {
        let mut statement = self.connection.prepare(
            "SELECT node, name, address, token, last_synced FROM peers WHERE node = ?1 AND removed = 0",
        )?;
        let mut rows = statement.query_map([node], row_to_peer)?;
        Ok(rows.next().transpose()?)
    }

    /// A peer this machine has dealt with itself. A token is only ever added,
    /// never dropped: hearing from a paired peer again without one in hand
    /// mustn't unpair it.
    pub fn remember_peer(&self, node: Id, name: &str, address: &str, token: Option<&str>) -> Result<()> {
        self.connection.execute(
            "INSERT INTO peers (node, name, address, token, removed, updated) VALUES (?1, ?2, ?3, ?4, 0, ?5)
             ON CONFLICT (node) DO UPDATE
             SET name = ?2, address = ?3, token = COALESCE(?4, token), removed = 0, updated = ?5",
            rusqlite::params![node, name, address, token, timestamp()],
        )?;
        Ok(())
    }

    pub fn remove_peer(&self, node: Id) -> Result<()> {
        self.connection.execute(
            "UPDATE peers SET removed = 1, token = NULL, updated = ?2 WHERE node = ?1",
            rusqlite::params![node, timestamp()],
        )?;
        Ok(())
    }

    pub fn was_removed(&self, node: Id) -> Result<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS (SELECT 1 FROM peers WHERE node = ?1 AND removed = 1)",
            [node],
            |row| row.get(0),
        )?)
    }

    pub fn mark_synced(&self, node: Id) -> Result<()> {
        self.connection.execute(
            "UPDATE peers SET last_synced = ?2 WHERE node = ?1",
            rusqlite::params![node, timestamp()],
        )?;
        Ok(())
    }

    pub fn peer_cards(&self) -> Result<Vec<PeerCard>> {
        let mut statement = self
            .connection
            .prepare("SELECT node, name, address, removed, updated FROM peers ORDER BY node")?;
        let rows = statement.query_map([], |row| {
            Ok(PeerCard {
                node: row.get(0)?,
                name: row.get(1)?,
                address: row.get(2)?,
                removed: row.get(3)?,
                updated: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn hear_of(&self, cards: &[PeerCard]) -> Result<()> {
        let me = self.node()?;
        for card in cards.iter().filter(|it| it.node != me) {
            self.connection.execute(
                "INSERT INTO peers (node, name, address, removed, updated) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (node) DO UPDATE SET name = ?2, address = ?3, removed = ?4, updated = ?5
                 WHERE ?5 > updated",
                rusqlite::params![card.node, card.name, card.address, card.removed, card.updated],
            )?;
        }
        Ok(())
    }

    /// A machine that isn't this person's by Tailscale's word is asking to be
    /// let in. Nothing happens until someone accepts its code here.
    pub fn request_pairing(&self, code: &str, node: Id, name: &str, address: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO pairings (code, node, name, address, requested) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (code) DO NOTHING",
            rusqlite::params![normalize_code(code), node, name, address, timestamp()],
        )?;
        Ok(())
    }

    pub fn accept_pairing(&self, code: &str) -> Result<Pairing> {
        let code = normalize_code(code);
        let pairing = self
            .pairing(&code)?
            .with_context(|| format!("nobody has asked to pair with code {code} — run `katami link up <this machine>` on the other one first"))?;
        if pairing.token.is_some() {
            bail!("{} was already accepted — it finishes pairing the next time it connects", pairing.name);
        }

        let token = new_token();
        self.connection.execute(
            "UPDATE pairings SET token = ?2 WHERE code = ?1",
            rusqlite::params![code, token],
        )?;
        Ok(Pairing { token: Some(token), ..pairing })
    }

    pub fn pairing(&self, code: &str) -> Result<Option<Pairing>> {
        let mut statement = self
            .connection
            .prepare("SELECT code, node, name, address, token FROM pairings WHERE code = ?1")?;
        let mut rows = statement.query_map([normalize_code(code)], row_to_pairing)?;
        Ok(rows.next().transpose()?)
    }

    pub fn pending_pairings(&self) -> Result<Vec<Pairing>> {
        let mut statement = self.connection.prepare(
            "SELECT code, node, name, address, token FROM pairings WHERE token IS NULL ORDER BY requested",
        )?;
        let rows = statement.query_map([], row_to_pairing)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn finish_pairing(&self, code: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM pairings WHERE code = ?1", [normalize_code(code)])?;
        Ok(())
    }
}

/// Eight characters a person can read out and type, shown as `K7M2-P9XQ`.
pub fn new_pairing_code() -> String {
    let code = Id::generate().as_str().to_uppercase();
    format!("{}-{}", &code[..4], &code[4..])
}

fn normalize_code(code: &str) -> String {
    code.chars().filter(|it| it.is_ascii_alphanumeric()).collect::<String>().to_uppercase()
}

fn new_token() -> String {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("the system's random source is unavailable");
    bytes.iter().map(|it| format!("{it:02x}")).collect()
}

fn row_to_peer(row: &rusqlite::Row) -> rusqlite::Result<Peer> {
    Ok(Peer {
        node: row.get(0)?,
        name: row.get(1)?,
        address: row.get(2)?,
        token: row.get(3)?,
        last_synced: row.get(4)?,
    })
}

fn row_to_pairing(row: &rusqlite::Row) -> rusqlite::Result<Pairing> {
    Ok(Pairing {
        code: row.get(0)?,
        node: row.get(1)?,
        name: row.get(2)?,
        address: row.get(3)?,
        token: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(node: &str, name: &str, address: &str, removed: bool, updated: &str) -> PeerCard {
        PeerCard {
            node: Id::parse(node).unwrap(),
            name: name.into(),
            address: address.into(),
            removed,
            updated: updated.into(),
        }
    }

    #[test]
    fn peers_are_heard_of_and_the_latest_card_wins() {
        let memory = Memory::open_in_memory().unwrap();
        let mini = Id::parse("mmmmmmmm").unwrap();

        memory.hear_of(&[card("mmmmmmmm", "mini", "100.64.0.2:5282", false, "2026-09-10T00:00:00Z")]).unwrap();
        memory.hear_of(&[card("mmmmmmmm", "mini", "100.64.0.9:5282", false, "2026-09-12T00:00:00Z")]).unwrap();
        memory.hear_of(&[card("mmmmmmmm", "stale", "100.64.0.1:5282", false, "2026-09-11T00:00:00Z")]).unwrap();

        let peer = memory.peer(mini).unwrap().unwrap();
        assert_eq!((peer.name.as_str(), peer.address.as_str()), ("mini", "100.64.0.9:5282"));
        assert_eq!(peer.token, None);

        let myself = card(memory.node().unwrap().as_str(), "me", "100.64.0.1:5282", false, "2026-09-20T00:00:00Z");
        memory.hear_of(&[myself]).unwrap();
        assert_eq!(memory.peers().unwrap().len(), 1);
    }

    #[test]
    fn a_removed_peer_stays_removed_until_it_is_linked_again() {
        let memory = Memory::open_in_memory().unwrap();
        let mini = Id::parse("mmmmmmmm").unwrap();
        memory.remember_peer(mini, "mini", "100.64.0.2:5282", Some("secret")).unwrap();

        memory.remove_peer(mini).unwrap();
        assert!(memory.peers().unwrap().is_empty());
        assert!(memory.peer_cards().unwrap()[0].removed);

        memory.hear_of(&[card("mmmmmmmm", "mini", "100.64.0.2:5282", false, "2026-01-01T00:00:00Z")]).unwrap();
        assert!(memory.peers().unwrap().is_empty());

        memory.remember_peer(mini, "mini", "100.64.0.2:5282", None).unwrap();
        assert_eq!(memory.peer(mini).unwrap().unwrap().token, None);
    }

    #[test]
    fn remembering_a_paired_peer_again_keeps_its_token() {
        let memory = Memory::open_in_memory().unwrap();
        let mini = Id::parse("mmmmmmmm").unwrap();
        memory.remember_peer(mini, "mini", "100.64.0.2:5282", Some("secret")).unwrap();
        memory.remember_peer(mini, "mini", "100.64.0.7:5282", None).unwrap();

        let peer = memory.peer(mini).unwrap().unwrap();
        assert_eq!(peer.token.as_deref(), Some("secret"));
        assert_eq!(peer.address, "100.64.0.7:5282");
    }

    #[test]
    fn a_pairing_waits_for_its_code_to_be_accepted() {
        let memory = Memory::open_in_memory().unwrap();
        let laptop = Id::parse("pppppppp").unwrap();
        let code = new_pairing_code();
        assert_eq!(code.len(), 9);

        assert!(memory.accept_pairing(&code).is_err());
        memory.request_pairing(&code, laptop, "laptop", "192.168.1.5:5282").unwrap();
        memory.request_pairing(&code, laptop, "laptop", "192.168.1.5:5282").unwrap();
        assert_eq!(memory.pending_pairings().unwrap().len(), 1);
        assert_eq!(memory.pairing(&code).unwrap().unwrap().token, None);

        let accepted = memory.accept_pairing(&code.to_lowercase().replace('-', " ")).unwrap();
        assert_eq!(accepted.node, laptop);
        assert_eq!(accepted.token.as_ref().map(String::len), Some(64));
        assert!(memory.pending_pairings().unwrap().is_empty());
        assert!(memory.accept_pairing(&code).is_err());

        memory.finish_pairing(&code).unwrap();
        assert_eq!(memory.pairing(&code).unwrap(), None);
    }
}
