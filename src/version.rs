//! Telling a newer version of a memory from a conflicting one.
//!
//! Timestamps can't: two machines can change the same memory in the same
//! minute, and "later wins" would quietly throw one change away. A version
//! vector can. It records, per node, the last write from that node this
//! version has absorbed, so comparing two says exactly one of four things:
//! they're the same, one grew out of the other — take the newer — or each
//! has something the other lacks, which is two machines writing at once and
//! calls for a merge rather than a winner.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::id::Id;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Version(BTreeMap<Id, u64>);

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Relation {
    Same,
    Newer,
    Older,
    Concurrent,
}

impl Version {
    pub fn parse(json: &str) -> anyhow::Result<Version> {
        Ok(serde_json::from_str(json)?)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a version vector is always valid JSON")
    }

    /// How this version stands to `other`: `Newer` means this one has
    /// absorbed everything the other has and more.
    pub fn relation_to(&self, other: &Version) -> Relation {
        let ahead = self.0.iter().any(|(node, seq)| *seq > other.absorbed(node));
        let behind = other.0.iter().any(|(node, seq)| *seq > self.absorbed(node));
        match (ahead, behind) {
            (false, false) => Relation::Same,
            (true, false) => Relation::Newer,
            (false, true) => Relation::Older,
            (true, true) => Relation::Concurrent,
        }
    }

    /// The version that has absorbed everything either one has.
    pub fn union(&self, other: &Version) -> Version {
        let mut union = self.0.clone();
        for (node, seq) in &other.0 {
            let absorbed = union.entry(*node).or_insert(0);
            *absorbed = (*absorbed).max(*seq);
        }
        Version(union)
    }

    fn absorbed(&self, node: &Id) -> u64 {
        self.0.get(node).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(json: &str) -> Version {
        Version::parse(json).unwrap()
    }

    #[test]
    fn versions_compare_by_what_each_has_absorbed() {
        let original = version(r#"{"aaaaaaaa":3}"#);
        let edited_on_a = version(r#"{"aaaaaaaa":5}"#);
        let edited_on_b = version(r#"{"aaaaaaaa":3,"bbbbbbbb":2}"#);

        assert_eq!(original.relation_to(&original), Relation::Same);
        assert_eq!(edited_on_a.relation_to(&original), Relation::Newer);
        assert_eq!(original.relation_to(&edited_on_a), Relation::Older);
        assert_eq!(edited_on_b.relation_to(&original), Relation::Newer);
        assert_eq!(edited_on_a.relation_to(&edited_on_b), Relation::Concurrent);
        assert_eq!(edited_on_b.relation_to(&edited_on_a), Relation::Concurrent);
        assert_eq!(Version::default().relation_to(&original), Relation::Older);
    }

    #[test]
    fn a_union_descends_from_both_sides_of_a_conflict() {
        let edited_on_a = version(r#"{"aaaaaaaa":5}"#);
        let edited_on_b = version(r#"{"aaaaaaaa":3,"bbbbbbbb":2}"#);

        let union = edited_on_a.union(&edited_on_b);
        assert_eq!(union, version(r#"{"aaaaaaaa":5,"bbbbbbbb":2}"#));
        assert_eq!(union.relation_to(&edited_on_a), Relation::Newer);
        assert_eq!(union.relation_to(&edited_on_b), Relation::Newer);
        assert_eq!(Version::parse(&union.to_json()).unwrap(), union);
    }
}
