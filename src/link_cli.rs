//! `katami link`: putting this machine's memory in a mesh with the others.
//!
//! Linking to any one machine is enough — peers introduce each other, so the
//! third machine only ever needs pointing at one of the first two. Between a
//! person's own Tailscale devices there's nothing else to do. A machine
//! Tailscale says belongs to someone else has to be paired: this side shows a
//! code and waits, and nothing is exchanged until a person types that code on
//! the other machine.

use anyhow::{Context, Result, bail};
use std::thread;
use std::time::Duration;

use crate::memory::Memory;
use crate::merger;
use crate::mesh::{self, Exchange, Reply};
use crate::paths;
use crate::peers::{self, Peer};

const PAIRING_POLL: Duration = Duration::from_secs(3);
const PAIRING_ATTEMPTS: usize = 100;

pub fn link(host: &str) -> Result<()> {
    let memory = open()?;
    let address = mesh::resolve(host)?;
    let known = memory.peers()?.into_iter().find(|it| it.address == address.to_string());
    let token = known.and_then(|it| it.token);

    if token.is_some() || mesh::trusted_by_tailscale(address.ip()) {
        match mesh::sync_with(&memory, host, token.as_deref(), None)? {
            Reply::Synced(exchange) => linked(&memory, &exchange),
            Reply::PairingPending => bail!("{host} wants to be paired again — run `katami link --remove {host}` and link it afresh"),
        }
    } else {
        pair(&memory, host)
    }
}

pub fn accept(code: &str) -> Result<()> {
    let pairing = open()?.accept_pairing(code)?;
    println!(
        "Accepted {} ({}). It finishes pairing by itself within a few seconds, as long as katami is listening here — a session, or `katami serve`.",
        pairing.name, pairing.address
    );
    Ok(())
}

pub fn remove(name: &str) -> Result<()> {
    let memory = open()?;
    let peer = find(&memory, name)?;
    memory.remove_peer(peer.node)?;
    println!(
        "Removed {} from the mesh. The other machines hear of it on their next sync; the memories already shared stay where they are.",
        peer.name
    );
    Ok(())
}

pub fn status() -> Result<()> {
    let memory = open()?;
    println!("This machine: {} (node {})", mesh::machine_name(), memory.node()?);

    let peers = memory.peers()?;
    if peers.is_empty() {
        println!("Not linked to any machine yet — `katami link <hostname|ip>` links it to one, and that one introduces the rest.");
    } else {
        println!("\n{:<20}  {:<24}  {:<10}  last synced", "machine", "address", "trusted by");
        for peer in &peers {
            println!(
                "{:<20}  {:<24}  {:<10}  {}",
                peer.name,
                peer.address,
                trust_of(peer),
                peer.last_synced.as_deref().unwrap_or("never")
            );
        }
    }

    let pending = memory.pending_pairings()?;
    if !pending.is_empty() {
        println!("\nAsking to pair — accept one with `katami link --accept <the code it shows>`:");
        for pairing in pending {
            println!("  {} ({})", pairing.name, pairing.address);
        }
    }

    let conflicted = memory.conflicted_ids()?.len();
    if conflicted > 0 {
        println!("\n{conflicted} memories changed on two machines at once and are waiting to be merged — it happens after the next session, or now with `katami memory sync`.");
    }
    Ok(())
}

pub fn serve() -> Result<()> {
    let listener = mesh::listen().context(
        "could not listen — Tailscale isn't up, or another katami on this machine already holds the port, in which case this machine can be linked to as it is",
    )?;
    println!("Listening on {} until interrupted.", listener.local_addr()?);
    mesh::serve(listener, paths::memory_dir());
    Ok(())
}

pub fn sync() -> Result<()> {
    if open()?.peers()?.is_empty() {
        bail!("not linked to any machine yet — run `katami link <hostname|ip>`");
    }

    let exchanges = mesh::sync_all()?;
    if exchanges.is_empty() {
        println!("No peer could be reached — see `katami log` for why, and `katami link` for who they are.");
    }
    for exchange in &exchanges {
        println!("{}", capitalize(&mesh::describe(exchange)));
    }
    settle_conflicts()
}

/// Asking for a sync means wanting to end up level, so conflicts are merged
/// on the spot and the merges sent straight back out.
fn settle_conflicts() -> Result<()> {
    let waiting = open()?.conflicted_ids()?.len();
    if waiting > 0 {
        println!("Merging {waiting} memories that changed on two machines at once…");
        merger::run(&paths::claude_config_home())?;

        let left = open()?.conflicted_ids()?.len();
        if left > 0 {
            println!("{left} could not be merged yet — see `katami log`.");
        }
        for exchange in mesh::sync_all()? {
            println!("{}", capitalize(&mesh::describe(&exchange)));
        }
    }
    Ok(())
}

fn pair(memory: &Memory, host: &str) -> Result<()> {
    let code = peers::new_pairing_code();
    println!("{host} isn't one of your machines by Tailscale's word, so it has to be paired.");
    println!("On {host}, run:\n\n    katami link --accept {code}\n\nWaiting for that…");

    for _ in 0..PAIRING_ATTEMPTS {
        if let Reply::Synced(exchange) = mesh::sync_with(memory, host, None, Some(&code))? {
            return linked(memory, &exchange);
        }
        thread::sleep(PAIRING_POLL);
    }
    bail!("nobody accepted {code} on {host} — run `katami link {host}` again when you're at it")
}

fn linked(memory: &Memory, exchange: &Exchange) -> Result<()> {
    println!("{}", capitalize(&mesh::describe(exchange)));

    let others: Vec<String> = memory
        .peers()?
        .into_iter()
        .filter(|it| it.node != exchange.peer)
        .map(|it| it.name)
        .collect();
    if !others.is_empty() {
        println!("It also knows {} — this machine syncs with them too from now on.", others.join(", "));
    }
    Ok(())
}

fn find(memory: &Memory, name: &str) -> Result<Peer> {
    let address = mesh::resolve(name).map(|it| it.to_string()).ok();
    memory
        .peers()?
        .into_iter()
        .find(|it| it.name.eq_ignore_ascii_case(name) || it.node.as_str() == name || Some(&it.address) == address.as_ref())
        .with_context(|| format!("no linked machine called {name} — see `katami link` for the ones there are"))
}

fn trust_of(peer: &Peer) -> &'static str {
    if peer.token.is_some() {
        "pairing"
    } else if mesh::resolve(&peer.address).is_ok_and(|it| mesh::trusted_by_tailscale(it.ip())) {
        "tailscale"
    } else {
        "nothing"
    }
}

fn capitalize(text: &str) -> String {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

fn open() -> Result<Memory> {
    Memory::open(&paths::memory_dir())
}
