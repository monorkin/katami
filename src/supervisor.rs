//! The supervisor: keeps the session's terminal honest while agent stays alive
//! around it.
//!
//! The byte stream between the user and the wrapped tool is never parsed —
//! it's spliced verbatim. Everything agent learns arrives through hook events
//! relayed to a unix socket, not through the terminal, so the same supervisor
//! serves claude, codex, pi, and opencode (and any of them shelling out to
//! another). One thread pumps stdin into the pty master; the main loop polls
//! the master and the signal pipe, pumping output to stdout, mirroring window
//! resizes, and forwarding externally-delivered SIGTERM/SIGHUP to the child's
//! process group. Ctrl+C needs no forwarding: in raw mode it travels as a
//! byte for the child's line discipline.

use anyhow::{Context, Result};
use std::io::{BufReader, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use crate::cards;
use crate::curator;
use crate::hook_protocol::{self, HookRequest, Tool};
use crate::launches;
use crate::logs;
use crate::memory::{Kind, Memory};
use crate::paths;
use crate::pty::{self, RawGuard};
use crate::reviewer;
use crate::search;
use crate::transcript;

pub fn supervise(mut command: Command, launch_key: String) -> Result<i32> {
    let server = HookServer::start(launch_key)?;
    command.env(hook_protocol::SOCKET_ENV_VAR, &server.socket_path);

    // Raw mode comes first: if anything later fails, the guard's Drop still
    // restores the terminal, and the child is never left orphaned behind a
    // half-configured pty
    let raw = RawGuard::enable()?;
    let mut pty = pty::spawn(command)?;
    let signal_pipe = pty::install_signal_pipe()?;
    pty::mirror_window_size(&pty.master)?;

    spawn_stdin_pump(&pty.master)?;
    pump_output(&pty.master, &signal_pipe, pty.child.id())?;
    drop(raw);

    let status = pty.child.wait().context("could not reap the child")?;
    // Some tools fire no reliable end event (opencode has none; pi's is
    // best-effort). The ledger of every session we saw is the backstop: on
    // exit, give each one a final review. A claude session that already ran
    // SessionEnd just re-reviews an empty delta — a no-op.
    review_seen_sessions(&server.context);
    drop(server);
    Ok(crate::launch::exit_code(&status))
}

fn review_seen_sessions(context: &ServerContext) {
    let sessions = context.sessions.lock().unwrap();
    for seen in sessions.values() {
        let _ = reviewer::spawn_final_review(&seen.source, &seen.config_dir, seen.cwd.as_deref());
    }
}

/// Serves `agent hook` clients for the lifetime of one session. The accept
/// loop runs on its own thread; each connection is one request/reply and is
/// handled inline — handlers must stay fast, the hook client gives up after
/// 1.5 seconds.
struct HookServer {
    socket_path: PathBuf,
    context: std::sync::Arc<ServerContext>,
}

struct ServerContext {
    launch_key: String,
    launch_recorded: std::sync::atomic::AtomicBool,
    sessions: std::sync::Mutex<std::collections::HashMap<String, SeenSession>>,
}

struct SeenSession {
    source: transcript::Source,
    cwd: Option<String>,
    config_dir: PathBuf,
}

impl HookServer {
    fn start(launch_key: String) -> Result<HookServer> {
        let directory = paths::runtime_dir();
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;
        assert_private_directory(&directory)?;
        thread::spawn(|| {
            let _ = crate::embeddings::embed("warm up the model before the first prompt");
        });

        let socket_path = directory.join(format!("{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("could not listen on {}", socket_path.display()))?;

        let context = std::sync::Arc::new(ServerContext {
            launch_key,
            launch_recorded: std::sync::atomic::AtomicBool::new(false),
            sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
        });
        let accept_context = context.clone();
        thread::spawn(move || {
            for connection in listener.incoming() {
                if let Ok(stream) = connection {
                    let context = accept_context.clone();
                    thread::spawn(move || serve_connection(stream, &context));
                }
            }
        });

        Ok(HookServer { socket_path, context })
    }
}

impl Drop for HookServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// On sticky-bit /tmp another user can pre-create our fallback directory and
/// swap the socket for their own — every hook would then relay the session to
/// them. Refuse anything we don't exclusively own, the way tmux does.
fn assert_private_directory(directory: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(directory)
        .with_context(|| format!("could not inspect {}", directory.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "{} is not a directory owned by this setup — remove it and relaunch",
            directory.display()
        );
    }
    if metadata.uid() != unsafe { libc::getuid() } {
        anyhow::bail!(
            "{} belongs to another user — remove it or set XDG_RUNTIME_DIR",
            directory.display()
        );
    }
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn serve_connection(stream: UnixStream, context: &ServerContext) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    let Ok(request) = hook_protocol::read_frame(&mut reader) else {
        return;
    };

    // Only claude frames carry a config dir, and only claude cares which one
    // it launched under (the reviewer's auth account, the skills dir)
    if request.tool == Tool::Claude
        && let Some(config_dir) = &request.config_dir
        && !context
            .launch_recorded
            .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        let _ = launches::record(&context.launch_key, config_dir);
    }
    remember_session(context, &request);

    let reply = handle_event(&request);
    let mut writer = stream;
    let _ = hook_protocol::write_reply(&mut writer, &reply);
}

fn remember_session(context: &ServerContext, request: &HookRequest) {
    let Some(source) = source_of(request) else {
        return;
    };
    let seen = SeenSession {
        source,
        cwd: cwd_of(request).map(str::to_string),
        config_dir: config_dir(request),
    };
    if let Ok(mut sessions) = context.sessions.lock() {
        sessions.insert(seen.source.cursor_key(), seen);
    }
}

fn handle_event(request: &HookRequest) -> serde_json::Value {
    log_event(request);
    match dispatch(request) {
        Ok(Some(reply)) => reply,
        Ok(None) => serde_json::json!({}),
        Err(error) => {
            log_line(&format!("{} handler failed: {error:#}", request.event));
            serde_json::json!({})
        }
    }
}

fn dispatch(request: &HookRequest) -> Result<Option<serde_json::Value>> {
    match request.event.as_str() {
        "SessionStart" => on_session_start(request),
        "UserPromptSubmit" => on_user_prompt(request),
        "PostToolUse" => on_post_tool_use(request),
        "Stop" => on_stop(request),
        "SessionEnd" => on_session_end(request),
        _ => Ok(None),
    }
}

fn on_stop(request: &HookRequest) -> Result<Option<serde_json::Value>> {
    if let Some(source) = source_of(request) {
        reviewer::maybe_spawn(&source, &config_dir(request), cwd_of(request))?;
    }
    Ok(None)
}

fn on_session_end(request: &HookRequest) -> Result<Option<serde_json::Value>> {
    if let Some(source) = source_of(request) {
        reviewer::spawn_final_review(&source, &config_dir(request), cwd_of(request))?;
    }
    curator::maybe_spawn(&config_dir(request))?;
    Ok(None)
}

/// The transcript a frame refers to. Claude, codex, and pi carry a file path;
/// opencode carries a session id (its conversation lives in SQLite).
fn source_of(request: &HookRequest) -> Option<transcript::Source> {
    match request.tool {
        Tool::Opencode => request.payload["session_id"]
            .as_str()
            .map(|id| transcript::Source::Opencode { session_id: id.to_string() }),
        tool => request.payload["transcript_path"].as_str().map(|path| {
            transcript::Source::File { tool, path: PathBuf::from(path) }
        }),
    }
}

fn cwd_of(request: &HookRequest) -> Option<&str> {
    request.payload["cwd"].as_str()
}

/// The reviewer and curator need a claude config dir for their `claude -p`
/// runs; only claude frames carry one, so everything else falls back to the
/// user's default login.
fn config_dir(request: &HookRequest) -> PathBuf {
    request
        .config_dir
        .clone()
        .unwrap_or_else(paths::claude_config_home)
}

fn on_session_start(request: &HookRequest) -> Result<Option<serde_json::Value>> {
    let memory = Memory::open(&paths::memory_dir())?;
    let all = memory.list()?;
    let mut included: Vec<i64> = Vec::new();
    let mut sections = Vec::new();

    if let Some(cwd) = request.payload["cwd"].as_str() {
        let entity = cards::canonical_project_entity(Path::new(cwd));
        memory.record_alias(&cards::project_entity(Path::new(cwd)), &entity)?;
        for card in all.iter().filter(|it| it.kind == Kind::Card) {
            if card.entity.as_deref() == Some(entity.as_str()) {
                included.push(card.id);
                sections.push(format!("## {}\n{}", card.title, card.body.trim()));
            }
        }
        if let Some(status) = memory.status_for_entity(&entity)? {
            included.push(status.id);
            sections.push(format!(
                "## Current state (as of {} — may be stale)\n{}",
                &status.updated[..10],
                status.body.trim()
            ));
        }
    }
    let pinned_ids: Vec<i64> = all
        .iter()
        .filter(|it| it.pinned && !included.contains(&it.id))
        .map(|it| it.id)
        .collect();
    for pinned in all.iter().filter(|it| pinned_ids.contains(&it.id)) {
        included.push(pinned.id);
        sections.push(format!("## {}\n{}", pinned.title, pinned.body.trim()));
    }

    if sections.is_empty() {
        return Ok(None);
    }
    record_deliveries(request, "session_start", included, Vec::new());
    let context = format!("Memories about this project:\n\n{}", sections.join("\n\n"));
    Ok(Some(context_reply(&context)))
}

fn on_user_prompt(request: &HookRequest) -> Result<Option<serde_json::Value>> {
    let prompt = request.payload["prompt"].as_str().unwrap_or("");
    if prompt.starts_with('/') || prompt.split_whitespace().count() < 3 {
        return Ok(None);
    }

    let memory = Memory::open(&paths::memory_dir())?;
    let hits = search::hybrid(&memory, prompt, 5)?;
    let Some(composed) = search::compose_context(&memory, &hits)? else {
        return Ok(None);
    };

    record_deliveries(request, "prompt", composed.full_ids.clone(), composed.pointer_ids.clone());
    Ok(Some(context_reply(&composed.text)))
}

/// The delivery manifest is bookkeeping, and bookkeeping happens off the
/// reply path — the hook client only waits 1500ms, and a reviewer holding
/// the write lock must not eat that budget.
fn record_deliveries(request: &HookRequest, event: &'static str, full: Vec<i64>, pointers: Vec<i64>) {
    let session = request.payload["session_id"].as_str().unwrap_or("?").to_string();
    thread::spawn(move || {
        let Ok(memory) = Memory::open(&paths::memory_dir()) else {
            return;
        };
        for id in full {
            let _ = memory.record_delivery(id, &session, event, "full");
            log_line(&format!("delivered memory {id} in full for session={session}"));
        }
        for id in pointers {
            let _ = memory.record_delivery(id, &session, event, "pointer");
        }
    });
}

fn on_post_tool_use(request: &HookRequest) -> Result<Option<serde_json::Value>> {
    let session = request.payload["session_id"].as_str().unwrap_or("?");
    let tool = request.payload["tool_name"].as_str().unwrap_or("");

    let skill = match tool {
        "Skill" => request.payload["tool_input"]["skill"]
            .as_str()
            .map(str::to_string),
        "Read" => request.payload["tool_input"]["file_path"]
            .as_str()
            .and_then(skill_read_from_path),
        _ => None,
    };

    if let Some(skill) = skill {
        let memory = Memory::open(&paths::memory_dir())?;
        memory.record_usage("skill", &skill, session)?;
    }
    Ok(None)
}

/// A Read inside any `skills/<name>/` directory counts as using that skill.
fn skill_read_from_path(path: &str) -> Option<String> {
    let components: Vec<&str> = Path::new(path)
        .components()
        .filter_map(|it| it.as_os_str().to_str())
        .collect();
    let position = components.iter().position(|it| *it == "skills")?;
    components.get(position + 1).map(|it| it.to_string())
}

/// The supervisor speaks one canonical reply; each edge (the Rust client for
/// claude and codex, the TS relays for pi and opencode) formats it into its
/// tool's native shape and applies the tool's own self-ingestion guard.
fn context_reply(context: &str) -> serde_json::Value {
    serde_json::json!({ "context": context })
}

fn log_event(request: &HookRequest) {
    log_line(&format!(
        "{} tool={} session={}",
        request.event,
        request.tool.as_str(),
        request.payload["session_id"].as_str().unwrap_or("?")
    ));
}

fn log_line(message: &str) {
    logs::append(&format!("supervisor-{}", std::process::id()), message);
}

fn spawn_stdin_pump(master: &OwnedFd) -> Result<()> {
    let master = master.try_clone().context("could not duplicate the pty")?;
    thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0u8; 4096];
        loop {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if write_all_to_fd(&master, &buffer[..count]).is_err() {
                        break;
                    }
                }
            }
        }
    });
    Ok(())
}

fn pump_output(master: &OwnedFd, signal_pipe: &OwnedFd, child_pid: u32) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let mut buffer = [0u8; 16384];

    loop {
        let mut fds = [
            libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: signal_pipe.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("could not poll the pty");
        }

        if fds[1].revents & libc::POLLIN != 0 {
            for signal in drain_signals(signal_pipe) {
                match signal {
                    libc::SIGWINCH => {
                        let _ = pty::mirror_window_size(master);
                    }
                    other => unsafe {
                        libc::kill(-(child_pid as libc::pid_t), other);
                    },
                }
            }
        }

        if fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            match read_from_fd(master, &mut buffer) {
                // EOF or EIO: the child closed its side — the session is over
                Ok(0) | Err(_) => return Ok(()),
                Ok(count) => {
                    stdout
                        .write_all(&buffer[..count])
                        .and_then(|()| stdout.flush())
                        .context("could not write to the terminal")?;
                }
            }
        }
    }
}

fn drain_signals(pipe: &OwnedFd) -> Vec<libc::c_int> {
    let mut signals = Vec::new();
    let mut byte = 0u8;
    loop {
        let count = unsafe {
            libc::read(
                pipe.as_raw_fd(),
                &mut byte as *mut u8 as *mut libc::c_void,
                1,
            )
        };
        if count != 1 {
            break;
        }
        signals.push(byte as libc::c_int);
    }
    signals
}

fn read_from_fd(fd: &OwnedFd, buffer: &mut [u8]) -> std::io::Result<usize> {
    let count = unsafe {
        libc::read(
            fd.as_raw_fd(),
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len(),
        )
    };
    if count < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}

fn write_all_to_fd(fd: &OwnedFd, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        let count = unsafe {
            libc::write(
                fd.as_raw_fd(),
                bytes.as_ptr() as *const libc::c_void,
                bytes.len(),
            )
        };
        if count < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        bytes = &bytes[count as usize..];
    }
    Ok(())
}

