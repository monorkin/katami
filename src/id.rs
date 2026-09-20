//! One id per memory, good on every machine.
//!
//! A counter can only be unique where it's counted, and memory is shared
//! between machines that never ask each other for a number. So an id is
//! random instead: eight characters, forty bits, which for a store of
//! thousands of memories collides about as often as never. It's short enough
//! to type, and the CLI takes any unique prefix the way git does.
//!
//! The alphabet is Crockford's base32 — no i, l, o, or u to misread — and the
//! first character is always a letter. A model asked to copy `[id 40213957]`
//! back into JSON will happily write it as a number; it can't do that to
//! `k0213957`.

use anyhow::{Result, bail};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const LETTERS: &[u8] = b"abcdefghjkmnpqrstvwxyz";
pub const LENGTH: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id([u8; LENGTH]);

impl Id {
    pub fn generate() -> Id {
        let mut random = [0u8; LENGTH];
        SystemRandom::new()
            .fill(&mut random)
            .expect("the system's random source is unavailable");

        let mut characters = [0u8; LENGTH];
        characters[0] = LETTERS[random[0] as usize % LETTERS.len()];
        for (character, byte) in characters.iter_mut().zip(random).skip(1) {
            *character = ALPHABET[byte as usize % ALPHABET.len()];
        }
        Id(characters)
    }

    pub fn parse(text: &str) -> Result<Id> {
        let bytes = text.as_bytes();
        if bytes.len() == LENGTH
            && LETTERS.contains(&bytes[0])
            && bytes.iter().all(|it| ALPHABET.contains(it))
        {
            Ok(Id(bytes.try_into().expect("the length was just checked")))
        } else {
            bail!("`{text}` is not a memory id — ids are {LENGTH} characters like `k7m2p9xq`")
        }
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("ids are ASCII")
    }
}

impl std::fmt::Display for Id {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.pad(self.as_str())
    }
}

impl std::fmt::Debug for Id {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(formatter, "Id({})", self.as_str())
    }
}

impl FromSql for Id {
    fn column_result(value: ValueRef) -> FromSqlResult<Self> {
        Id::parse(value.as_str()?).map_err(|it| FromSqlError::Other(it.into()))
    }
}

impl ToSql for Id {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Id, D::Error> {
        let text = String::deserialize(deserializer)?;
        Id::parse(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_parse_back_and_never_look_like_numbers() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2000 {
            let id = Id::generate();
            assert_eq!(Id::parse(id.as_str()).unwrap(), id);
            assert!(id.as_str().as_bytes()[0].is_ascii_lowercase());
            assert!(seen.insert(id), "two of 2000 ids collided");
        }
    }

    #[test]
    fn only_well_formed_ids_parse() {
        assert!(Id::parse("k7m2p9xq").is_ok());
        for malformed in ["", "k7m2p9x", "k7m2p9xqq", "17m2p9xq", "k7m2p9xo", "K7M2P9XQ", "k7m2 9xq"] {
            assert!(Id::parse(malformed).is_err(), "{malformed}");
        }
    }

    #[test]
    fn ids_travel_through_json_as_strings() {
        let id = Id::parse("k7m2p9xq").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"k7m2p9xq\"");
        assert_eq!(serde_json::from_str::<Id>("\"k7m2p9xq\"").unwrap(), id);
        assert!(serde_json::from_str::<Id>("40213957").is_err());
    }
}
