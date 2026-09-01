//! The supervisor: keeps the session's terminal honest while agent stays alive
//! around it.
//!
//! The byte stream between the user and claude is never parsed — it's spliced
//! verbatim. Everything agent learns about the session arrives through hook
//! events, not through the terminal. One thread pumps stdin into the pty
//! master; the main loop polls the master and the signal pipe, pumping output
//! to stdout, mirroring window resizes, and forwarding externally-delivered
//! SIGTERM/SIGHUP to the child's process group. Ctrl+C needs no forwarding:
//! in raw mode it travels as a byte for the child's line discipline.

use anyhow::{Context, Result};
use std::fs::OpenOptions;
use std::io::{BufReader, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use crate::cards;
use crate::curator;
use crate::hook_protocol::{self, HookRequest};
use crate::launches;
use crate::memory::Memory;
use crate::paths;
use crate::pty::{self, RawGuard};
use crate::reviewer;
use crate::search;

pub const SOCKET_ENV_VAR: &str = "AGENT_HOOK_SOCKET";

pub fn supervise(mut command: Command, launch_key: String) -> Result<i32> {
    let server = HookServer::start(launch_key)?;
    command.env(SOCKET_ENV_VAR, &server.socket_path);

    let mut pty = pty::spawn(command)?;
    let signal_pipe = pty::install_signal_pipe()?;
    pty::mirror_window_size(&pty.master)?;

    let raw = RawGuard::enable()?;
    spawn_stdin_pump(&pty.master)?;
    pump_output(&pty.master, &signal_pipe, pty.child.id())?;
    drop(raw);

    let status = pty.child.wait().context("could not reap claude")?;
    drop(server);
    Ok(exit_code(&status))
}

/// Serves `agent hook` clients for the lifetime of one session. The accept
/// loop runs on its own thread; each connection is one request/reply and is
/// handled inline — handlers must stay fast, the hook client gives up after
/// 1.5 seconds.
struct HookServer {
    socket_path: PathBuf,
}

struct ServerContext {
    launch_key: String,
    launch_recorded: std::sync::atomic::AtomicBool,
}

impl HookServer {
    fn start(launch_key: String) -> Result<HookServer> {
        let directory = paths::runtime_dir();
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("could not create {}", directory.display()))?;

        let socket_path = directory.join(format!("{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("could not listen on {}", socket_path.display()))?;

        let context = std::sync::Arc::new(ServerContext {
            launch_key,
            launch_recorded: std::sync::atomic::AtomicBool::new(false),
        });
        thread::spawn(move || {
            for connection in listener.incoming() {
                if let Ok(stream) = connection {
                    let context = context.clone();
                    thread::spawn(move || serve_connection(stream, &context));
                }
            }
        });

        Ok(HookServer { socket_path })
    }
}

impl Drop for HookServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn serve_connection(stream: UnixStream, context: &ServerContext) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    let Ok(request) = hook_protocol::read_frame(&mut reader) else {
        return;
    };

    if !context
        .launch_recorded
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        let _ = launches::record(&context.launch_key, &request.config_dir);
    }

    let reply = handle_event(&request);
    let mut writer = stream;
    let _ = hook_protocol::write_reply(&mut writer, &reply);
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
        "Stop" => on_stop(request, false),
        "SessionEnd" => on_stop(request, true),
        _ => Ok(None),
    }
}

fn on_stop(request: &HookRequest, force: bool) -> Result<Option<serde_json::Value>> {
    if let Some(transcript) = request.payload["transcript_path"].as_str() {
        reviewer::maybe_spawn(transcript, &request.config_dir, force)?;
    }
    if force {
        curator::maybe_spawn(&request.config_dir)?;
    }
    Ok(None)
}

fn on_session_start(request: &HookRequest) -> Result<Option<serde_json::Value>> {
    let memory = Memory::open(&paths::memory_dir())?;
    let mut sections = Vec::new();

    if let Some(cwd) = request.payload["cwd"].as_str() {
        let entity = cards::project_entity(Path::new(cwd));
        for card in memory.list()? {
            if card.kind == "card" && card.entity.as_deref() == Some(entity.as_str()) {
                sections.push(format!("## {}\n{}", card.title, card.body.trim()));
            }
        }
    }
    for pinned in memory.list()?.iter().filter(|it| it.pinned) {
        sections.push(format!("## {}\n{}", pinned.title, pinned.body.trim()));
    }

    if sections.is_empty() {
        return Ok(None);
    }
    let context = format!("Memories about this project:\n\n{}", sections.join("\n\n"));
    Ok(Some(additional_context("SessionStart", &context)))
}

fn on_user_prompt(request: &HookRequest) -> Result<Option<serde_json::Value>> {
    let prompt = request.payload["prompt"].as_str().unwrap_or("");
    if prompt.starts_with('/') || prompt.split_whitespace().count() < 3 {
        return Ok(None);
    }

    let memory = Memory::open(&paths::memory_dir())?;
    let hits = search::hybrid(&memory, prompt, 5)?;
    let session = request.payload["session_id"].as_str().unwrap_or("?");
    for hit in &hits {
        let stored = memory.get(hit.id)?;
        memory.record_usage("memory", &stored.title, session)?;
        log_line(&format!("injected [[{}]] for session={session}", stored.title));
    }

    match search::compose_context(&memory, &hits)? {
        Some(context) => Ok(Some(additional_context("UserPromptSubmit", &context))),
        None => Ok(None),
    }
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

fn additional_context(event: &str, context: &str) -> serde_json::Value {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event,
            "additionalContext": context
        }
    })
}

fn log_event(request: &HookRequest) {
    log_line(&format!(
        "{} config_dir={} session={}",
        request.event,
        request.config_dir.display(),
        request.payload["session_id"].as_str().unwrap_or("?")
    ));
}

fn log_line(message: &str) {
    let path = paths::logs_dir().join(format!("supervisor-{}.log", std::process::id()));
    if std::fs::create_dir_all(paths::logs_dir()).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {message}", crate::timestamp());
    }
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

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        1
    }
}
