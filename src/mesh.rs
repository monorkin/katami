//! Stores talking to each other: the wire, who gets let in, and when.
//!
//! One connection is one whole sync. The caller says hello, and once it's
//! admitted it pulls everything it's missing and then pushes everything the
//! other side is missing, so whichever machine happens to dial, both end up
//! level. Frames are newline-delimited JSON, like the hook socket.
//!
//! Nothing runs when katami doesn't: the supervisor is the listener, bound to
//! this machine's Tailscale address and nothing else, and it syncs with every
//! peer when a session starts and every few minutes after. The reviewer and
//! curator sync when they finish, since that's when memories are made. So
//! memories move whenever two machines are in use around the same time, or
//! through any machine that's always in a session — and no machine is one the
//! others depend on. Several supervisors on one machine share the port by
//! whoever binds first; the rest keep trying, and take over when it exits.
//!
//! A caller is admitted if Tailscale says its device belongs to the same
//! user, or if it holds the token from a pairing. Anyone else can only ask to
//! be paired, which does nothing until a person accepts the code on this
//! machine. A peer that was removed stays out until someone links it again.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::fsutil;
use crate::id::Id;
use crate::logs;
use crate::memory::Memory;
use crate::paths;
use crate::peers::{Peer, PeerCard};
use crate::replica::{Delta, Knowledge, Tally};
use crate::shared::SharedValue;
use crate::tailscale;
use crate::transfer;

const PROTOCOL: u32 = 1;
const DEFAULT_PORT: u16 = 5282;
const LISTEN_OVERRIDE: &str = "KATAMI_MESH_LISTEN";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const IO_TIMEOUT: Duration = Duration::from_secs(60);
const SYNC_INTERVAL: Duration = Duration::from_secs(300);
const HELLO_BYTES_LIMIT: u64 = 64 * 1024;
const PENDING_PAIRINGS_LIMIT: usize = 20;

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Message {
    Hello(Hello),
    Welcome { node: Id, name: String },
    Paired { node: Id, name: String, token: String },
    PairingPending,
    Refused { reason: String },
    Pull { knowledge: Knowledge, gossip: Gossip },
    Delta { delta: Delta, gossip: Gossip },
    Push { delta: Delta },
    Done,
}

/// What rides along with the memories and their logs: who's in the mesh, and
/// the little state it shares. Small enough to send whole every time.
#[derive(Serialize, Deserialize, Debug)]
struct Gossip {
    peers: Vec<PeerCard>,
    shared: Vec<SharedValue>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Hello {
    protocol: u32,
    node: Id,
    name: String,
    listen_port: u16,
    token: Option<String>,
    pairing_code: Option<String>,
}

#[derive(Debug)]
pub enum Reply {
    Synced(Exchange),
    PairingPending,
}

#[derive(Debug)]
pub struct Exchange {
    pub peer: Id,
    pub name: String,
    pub received: Tally,
    pub sent: usize,
}

/// Holds the port if nobody on this machine does, and keeps this store level
/// with its peers for as long as the supervisor lives.
pub fn start() {
    thread::spawn(|| {
        let mut listening = false;
        loop {
            if !listening && let Some(listener) = listen() {
                listening = true;
                thread::spawn(move || serve(listener, paths::memory_dir()));
            }
            sync_all_quietly();
            thread::sleep(SYNC_INTERVAL);
        }
    });
}

pub fn listen() -> Option<TcpListener> {
    let address = match std::env::var(LISTEN_OVERRIDE) {
        Ok(address) => address.parse().ok()?,
        Err(_) => SocketAddr::new(tailscale::status()?.my_address()?, port()),
    };
    TcpListener::bind(address).ok()
}

pub fn serve(listener: TcpListener, store: PathBuf) {
    log(&format!("listening on {}", listener.local_addr().map(|it| it.to_string()).unwrap_or_default()));
    for connection in listener.incoming() {
        if let Ok(stream) = connection {
            let store = store.clone();
            thread::spawn(move || {
                if let Err(error) = serve_connection(stream, &store) {
                    log(&format!("a caller was turned away or dropped: {error:#}"));
                }
            });
        }
    }
}

pub fn sync_all_quietly() {
    match sync_all() {
        Ok(exchanges) => {
            for exchange in exchanges {
                log(&describe(&exchange));
            }
        }
        Err(error) => log(&format!("could not sync: {error:#}")),
    }
}

/// Every peer that can be reached gets one sync; one that can't is no
/// reason to skip the rest.
pub fn sync_all() -> Result<Vec<Exchange>> {
    let memory = Memory::open(&paths::memory_dir())?;
    let mut exchanges = Vec::new();
    for peer in memory.peers()? {
        match sync_with_peer(&memory, &peer) {
            Ok(Reply::Synced(exchange)) => exchanges.push(exchange),
            Ok(Reply::PairingPending) => log(&format!("{} still has to accept a pairing", peer.name)),
            Err(error) => log(&format!("could not sync with {}: {error:#}", peer.name)),
        }
    }
    Ok(exchanges)
}

/// A peer heard of second-hand is only dialed if this machine would trust it
/// first-hand: everything this store knows is about to be sent there.
fn sync_with_peer(memory: &Memory, peer: &Peer) -> Result<Reply> {
    let address = resolve(&peer.address)?;
    if peer.token.is_some() || trusted_by_tailscale(address.ip()) {
        sync_with(memory, &peer.address, peer.token.as_deref(), None)
    } else {
        bail!("it isn't one of your machines by Tailscale's word and was never paired — run `katami link up {}`", peer.address)
    }
}

pub fn sync_with(memory: &Memory, address: &str, token: Option<&str>, pairing_code: Option<&str>) -> Result<Reply> {
    let address = resolve(address)?;
    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT).with_context(|| {
        format!("could not reach {address} — katami listens there while a session is running on that machine, or under `katami serve`")
    })?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    send(&mut writer, &Message::Hello(Hello {
        protocol: PROTOCOL,
        node: memory.node()?,
        name: machine_name(),
        listen_port: port(),
        token: token.map(str::to_string),
        pairing_code: pairing_code.map(str::to_string),
    }))?;

    let (peer, name, granted) = match receive(&mut reader)? {
        Message::Welcome { node, name } => (node, name, None),
        Message::Paired { node, name, token } => (node, name, Some(token)),
        Message::PairingPending => return Ok(Reply::PairingPending),
        Message::Refused { reason } => bail!("{address} refused: {reason}"),
        other => bail!("{address} answered a hello with {other:?}"),
    };
    memory.remember_peer(peer, &name, &address.to_string(), granted.as_deref())?;

    send(&mut writer, &Message::Pull { knowledge: memory.knowledge()?, gossip: gossip_of(memory)? })?;
    let Message::Delta { delta, gossip } = receive(&mut reader)? else {
        bail!("{name} did not answer the pull with a delta");
    };
    hear(memory, &gossip)?;
    let received = memory.absorb_delta(&delta)?;

    let ours = memory.delta_for(&delta.knowledge)?;
    let sent = ours.records.len();
    send(&mut writer, &Message::Push { delta: ours })?;
    let Message::Done = receive(&mut reader)? else {
        bail!("{name} did not confirm the push");
    };

    transfer::refresh_derived(memory, &received.changed)?;
    memory.mark_synced(peer)?;
    Ok(Reply::Synced(Exchange { peer, name, received, sent }))
}

pub fn describe(exchange: &Exchange) -> String {
    let mut description = format!(
        "synced with {}: received {}, sent {}",
        exchange.name,
        exchange.received.changed.len(),
        exchange.sent
    );
    if !exchange.received.conflicted.is_empty() {
        description.push_str(&format!(
            ", {} changed on both machines at once and will be merged",
            exchange.received.conflicted.len()
        ));
    }
    description
}

pub fn port() -> u16 {
    fsutil::read_json(&paths::data_dir().join("config.json"))
        .ok()
        .and_then(|it| it["mesh_port"].as_u64())
        .and_then(|it| u16::try_from(it).ok())
        .unwrap_or(DEFAULT_PORT)
}

/// `host`, `host:port`, or an address — a bare host is looked up on the
/// tailnet first, so `katami link up mini` works without MagicDNS.
pub fn resolve(address: &str) -> Result<SocketAddr> {
    if let Ok(exact) = address.parse::<SocketAddr>() {
        return Ok(exact);
    }
    if let Ok(ip) = address.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port()));
    }

    let (host, port) = match address.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => (host, port.parse().expect("just checked")),
        _ => (address, port()),
    };
    if let Some(ip) = tailscale::status().and_then(|it| it.address_of(host)) {
        Ok(SocketAddr::new(ip, port))
    } else {
        (host, port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .with_context(|| format!("no machine called {host} on your tailnet or in DNS — try its Tailscale IP"))
    }
}

pub fn trusted_by_tailscale(address: IpAddr) -> bool {
    tailscale::status().is_some_and(|it| it.is_mine(address))
}

fn serve_connection(stream: TcpStream, store: &Path) -> Result<()> {
    let remote = stream.peer_addr()?.ip();
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let memory = Memory::open(store)?;

    let mut greeting = String::new();
    BufReader::new(stream.try_clone()?.take(HELLO_BYTES_LIMIT)).read_line(&mut greeting)?;
    let Message::Hello(hello) = serde_json::from_str(&greeting).context("the caller did not open with a hello")? else {
        bail!("the caller did not open with a hello");
    };

    let admission = admit(&memory, &hello, remote)?;
    let admitted = matches!(admission, Message::Welcome { .. } | Message::Paired { .. });
    send(&mut writer, &admission)?;
    if !admitted {
        bail!("{} at {remote} was not admitted: {admission:?}", hello.name);
    }

    let mut reader = BufReader::new(stream);
    let Message::Pull { knowledge, gossip } = receive(&mut reader)? else {
        bail!("{} did not pull after its hello", hello.name);
    };
    hear(&memory, &gossip)?;
    send(&mut writer, &Message::Delta { delta: memory.delta_for(&knowledge)?, gossip: gossip_of(&memory)? })?;

    let Message::Push { delta } = receive(&mut reader)? else {
        bail!("{} did not push after its pull", hello.name);
    };
    let received = memory.absorb_delta(&delta)?;
    send(&mut writer, &Message::Done)?;

    transfer::refresh_derived(&memory, &received.changed)?;
    memory.mark_synced(hello.node)?;
    log(&format!(
        "{} synced: received {}, {} in conflict",
        hello.name,
        received.changed.len(),
        received.conflicted.len()
    ));
    Ok(())
}

fn gossip_of(memory: &Memory) -> Result<Gossip> {
    Ok(Gossip {
        peers: memory.peer_cards()?,
        shared: memory.shared_values()?,
    })
}

fn hear(memory: &Memory, gossip: &Gossip) -> Result<()> {
    memory.hear_of(&gossip.peers)?;
    memory.hear_shared(&gossip.shared)
}

fn admit(memory: &Memory, hello: &Hello, remote: IpAddr) -> Result<Message> {
    let address = SocketAddr::new(remote, hello.listen_port).to_string();
    let welcome = Message::Welcome { node: memory.node()?, name: machine_name() };

    if hello.protocol != PROTOCOL {
        Ok(refused(&format!(
            "it speaks mesh protocol {PROTOCOL} and the caller speaks {} — bring both machines to the same katami with `katami upgrade`",
            hello.protocol
        )))
    } else if hello.node == memory.node()? {
        Ok(refused("that is this very store — link a different machine"))
    } else if memory.was_removed(hello.node)? {
        Ok(refused("this machine was removed from the mesh — run `katami link up` from the machine that removed it to bring it back"))
    } else if trusted_by_tailscale(remote) || holds_token(memory, hello)? {
        memory.remember_peer(hello.node, &hello.name, &address, None)?;
        Ok(welcome)
    } else if let Some(code) = &hello.pairing_code {
        pair(memory, hello, code, &address)
    } else {
        Ok(refused("the caller isn't one of this person's machines by Tailscale's word, and isn't paired — run `katami link up` toward this machine to pair it"))
    }
}

/// Digests are compared rather than the tokens themselves, so how long the
/// comparison takes says nothing about how much of a guess was right.
fn holds_token(memory: &Memory, hello: &Hello) -> Result<bool> {
    let digest = |it: &String| ring::digest::digest(&ring::digest::SHA256, it.as_bytes());
    let expected = memory.peer(hello.node)?.and_then(|it| it.token);
    Ok(match (&expected, &hello.token) {
        (Some(expected), Some(offered)) => digest(expected).as_ref() == digest(offered).as_ref(),
        _ => false,
    })
}

fn pair(memory: &Memory, hello: &Hello, code: &str, address: &str) -> Result<Message> {
    match memory.pairing(code)? {
        Some(pairing) if pairing.node == hello.node && pairing.token.is_some() => {
            let token = pairing.token.expect("just matched");
            memory.remember_peer(hello.node, &hello.name, address, Some(&token))?;
            memory.finish_pairing(code)?;
            Ok(Message::Paired { node: memory.node()?, name: machine_name(), token })
        }
        Some(_) => Ok(Message::PairingPending),
        None => {
            if memory.pending_pairings()?.len() < PENDING_PAIRINGS_LIMIT {
                memory.request_pairing(code, hello.node, &hello.name, address)?;
                log(&format!("{} at {address} asks to pair — accept with `katami link accept <the code it shows>`", hello.name));
            }
            Ok(Message::PairingPending)
        }
    }
}

fn refused(reason: &str) -> Message {
    Message::Refused { reason: reason.to_string() }
}

fn send(stream: &mut impl Write, message: &Message) -> Result<()> {
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    stream.write_all(line.as_bytes()).context("the connection dropped mid-sync")
}

fn receive(stream: &mut impl BufRead) -> Result<Message> {
    let mut line = String::new();
    if stream.read_line(&mut line)? == 0 {
        bail!("the connection closed mid-sync");
    }
    serde_json::from_str(&line).context("a mesh frame did not parse")
}

pub fn machine_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|it| it.trim().to_string())
        .ok()
        .filter(|it| !it.is_empty())
        .unwrap_or_else(|| "unnamed".to_string())
}

/// The log lives in the real data dir, which a test run has no business
/// writing to.
fn log(message: &str) {
    if cfg!(not(test)) {
        logs::append("mesh", message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Kind, NewMemory};
    use crate::peers::new_pairing_code;

    fn machine(name: &str) -> (Memory, PathBuf) {
        let directory = std::env::temp_dir().join(format!("katami-mesh-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        (Memory::open(&directory).unwrap(), directory)
    }

    fn listening(store: &Path) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let store = store.to_path_buf();
        thread::spawn(move || serve(listener, store));
        address
    }

    fn learn(memory: &Memory, title: &str) -> Id {
        memory
            .add(&NewMemory {
                kind: Kind::Observation,
                entity: None,
                title: title.into(),
                body: "Body.".into(),
                links: vec![],
                source_session: None,
                class: None,
            })
            .unwrap()
    }

    fn synced(reply: Reply) -> Exchange {
        match reply {
            Reply::Synced(exchange) => exchange,
            Reply::PairingPending => panic!("expected a sync, got a pending pairing"),
        }
    }

    #[test]
    fn a_stranger_gets_nothing_until_its_code_is_accepted_and_then_syncs_both_ways() {
        let (mini, mini_store) = machine("pairing-mini");
        let (laptop, laptop_store) = machine("pairing-laptop");
        let address = listening(&mini_store);
        let on_mini = learn(&mini, "Learned on the mini");
        let on_laptop = learn(&laptop, "Learned on the laptop");

        let refusal = sync_with(&laptop, &address, None, None).unwrap_err().to_string();
        assert!(refusal.contains("isn't paired"), "{refusal}");

        let code = new_pairing_code();
        assert!(matches!(sync_with(&laptop, &address, None, Some(&code)).unwrap(), Reply::PairingPending));
        assert!(matches!(sync_with(&laptop, &address, None, Some(&code)).unwrap(), Reply::PairingPending));
        assert!(!mini.exists(on_laptop).unwrap());
        assert!(!laptop.exists(on_mini).unwrap());
        assert_eq!(mini.pending_pairings().unwrap().len(), 1);

        mini.accept_pairing(&code).unwrap();
        let exchange = synced(sync_with(&laptop, &address, None, Some(&code)).unwrap());
        assert_eq!(exchange.peer, mini.node().unwrap());
        assert_eq!(exchange.received.changed, vec![on_mini]);
        assert_eq!(exchange.sent, 1);
        assert!(mini.exists(on_laptop).unwrap());
        assert_eq!(mini.pairing(&code).unwrap(), None);

        let token = laptop.peer(exchange.peer).unwrap().unwrap().token;
        assert!(token.is_some());
        assert_eq!(mini.peer(laptop.node().unwrap()).unwrap().unwrap().token, token);

        let later = learn(&laptop, "Learned later");
        let exchange = synced(sync_with(&laptop, &address, token.as_deref(), None).unwrap());
        assert_eq!((exchange.received.changed.len(), exchange.sent), (0, 1));
        assert!(mini.exists(later).unwrap());

        assert!(sync_with(&laptop, &address, Some("not the token"), None).is_err());

        for store in [mini_store, laptop_store] {
            std::fs::remove_dir_all(store).unwrap();
        }
    }

    #[test]
    fn a_removed_machine_is_turned_away_even_with_its_token() {
        let (mini, mini_store) = machine("removal-mini");
        let (laptop, laptop_store) = machine("removal-laptop");
        let address = listening(&mini_store);

        let code = new_pairing_code();
        sync_with(&laptop, &address, None, Some(&code)).unwrap();
        mini.accept_pairing(&code).unwrap();
        let exchange = synced(sync_with(&laptop, &address, None, Some(&code)).unwrap());
        let token = laptop.peer(exchange.peer).unwrap().unwrap().token;

        mini.remove_peer(laptop.node().unwrap()).unwrap();
        let refusal = sync_with(&laptop, &address, token.as_deref(), None).unwrap_err().to_string();
        assert!(refusal.contains("removed from the mesh"), "{refusal}");

        for store in [mini_store, laptop_store] {
            std::fs::remove_dir_all(store).unwrap();
        }
    }

    #[test]
    fn addresses_resolve_with_or_without_a_port() {
        assert_eq!(resolve("100.64.0.2:7000").unwrap().to_string(), "100.64.0.2:7000");
        assert_eq!(resolve("100.64.0.2").unwrap(), SocketAddr::new("100.64.0.2".parse().unwrap(), port()));
        assert_eq!(resolve("localhost:7000").unwrap().port(), 7000);
    }
}
