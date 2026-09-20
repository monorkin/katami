//! Tailscale as the mesh's network and its first line of trust.
//!
//! A tailnet already gives every machine a stable address, an encrypted path
//! to the others, and a verified owner. So linking needs no keys of its own
//! in the common case: a device owned by the same Tailscale user is one of
//! this person's machines, and trusted on sight. A tailnet is often shared —
//! a whole company can be on one — so a device owned by anyone else is not,
//! and has to be paired by hand.
//!
//! One `tailscale status --json` answers everything asked here: this
//! machine's address, who owns the device at a given address, and what
//! address a hostname has.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::process::Command;

pub struct Tailnet {
    me: Device,
    others: Vec<Device>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Device {
    #[serde(rename = "UserID")]
    user_id: u64,
    #[serde(default)]
    host_name: String,
    #[serde(default, rename = "DNSName")]
    dns_name: String,
    #[serde(default, rename = "TailscaleIPs")]
    addresses: Vec<IpAddr>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Status {
    #[serde(rename = "Self")]
    me: Device,
    #[serde(default)]
    peer: BTreeMap<String, Device>,
}

/// `None` when Tailscale isn't installed, isn't running, or isn't logged in —
/// which only means nothing is trusted on sight and nothing is listened on.
pub fn status() -> Option<Tailnet> {
    let output = Command::new("tailscale").args(["status", "--json"]).output().ok()?;
    if output.status.success() {
        Tailnet::parse(&String::from_utf8_lossy(&output.stdout)).ok()
    } else {
        None
    }
}

impl Tailnet {
    pub fn parse(json: &str) -> Result<Tailnet> {
        let status: Status = serde_json::from_str(json).context("`tailscale status --json` did not parse")?;
        Ok(Tailnet {
            me: status.me,
            others: status.peer.into_values().collect(),
        })
    }

    pub fn my_address(&self) -> Option<IpAddr> {
        self.me.addresses.iter().find(|it| it.is_ipv4()).or(self.me.addresses.first()).copied()
    }

    pub fn is_mine(&self, address: IpAddr) -> bool {
        self.me.addresses.contains(&address)
            || self
                .others
                .iter()
                .any(|it| it.user_id == self.me.user_id && it.addresses.contains(&address))
    }

    /// The tailnet address of a machine named the way a person would name it:
    /// its hostname, or its MagicDNS name with or without the tailnet suffix.
    pub fn address_of(&self, name: &str) -> Option<IpAddr> {
        let wanted = name.trim_end_matches('.').to_lowercase();
        self.others
            .iter()
            .find(|it| {
                let dns_name = it.dns_name.trim_end_matches('.').to_lowercase();
                it.host_name.to_lowercase() == wanted
                    || dns_name == wanted
                    || dns_name.split('.').next() == Some(wanted.as_str())
            })
            .and_then(|it| it.addresses.iter().find(|it| it.is_ipv4()).or(it.addresses.first()).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = r#"{
        "Self": {"UserID": 100, "HostName": "desk", "DNSName": "desk.tail0000.ts.net.",
                 "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"]},
        "Peer": {
            "nodekey:aa": {"UserID": 100, "HostName": "Closet Mini", "DNSName": "closet-mini.tail0000.ts.net.",
                           "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"], "Online": true},
            "nodekey:bb": {"UserID": 200, "HostName": "colleagues-laptop", "DNSName": "colleagues-laptop.tail0000.ts.net.",
                           "TailscaleIPs": ["100.64.0.3"], "Online": false}
        }
    }"#;

    #[test]
    fn only_devices_owned_by_the_same_user_are_mine() {
        let tailnet = Tailnet::parse(STATUS).unwrap();
        assert_eq!(tailnet.my_address(), Some("100.64.0.1".parse().unwrap()));
        assert!(tailnet.is_mine("100.64.0.1".parse().unwrap()));
        assert!(tailnet.is_mine("100.64.0.2".parse().unwrap()));
        assert!(tailnet.is_mine("fd7a:115c:a1e0::2".parse().unwrap()));
        assert!(!tailnet.is_mine("100.64.0.3".parse().unwrap()));
        assert!(!tailnet.is_mine("192.168.1.5".parse().unwrap()));
    }

    #[test]
    fn machines_are_found_by_hostname_or_magic_dns_name() {
        let tailnet = Tailnet::parse(STATUS).unwrap();
        let mini = Some("100.64.0.2".parse().unwrap());
        assert_eq!(tailnet.address_of("closet-mini"), mini);
        assert_eq!(tailnet.address_of("Closet Mini"), mini);
        assert_eq!(tailnet.address_of("closet-mini.tail0000.ts.net"), mini);
        assert_eq!(tailnet.address_of("nonexistent"), None);
    }

    #[test]
    fn a_status_without_peers_still_parses() {
        let alone = Tailnet::parse(r#"{"Self": {"UserID": 1, "TailscaleIPs": ["100.64.0.9"]}}"#).unwrap();
        assert!(alone.is_mine("100.64.0.9".parse().unwrap()));
        assert_eq!(alone.address_of("anything"), None);
    }
}
