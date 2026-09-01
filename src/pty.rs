//! PTY plumbing: a pseudo-terminal for the wrapped claude, raw mode for ours.
//!
//! Claude Code is a full TUI — it needs to believe it owns a terminal, and the
//! user's terminal has to get out of the way. So the child runs on a PTY slave
//! as its controlling terminal, while the user's tty goes raw so every
//! keystroke (Ctrl+C included) travels through as bytes for the child's line
//! discipline to interpret, not ours.

use anyhow::{Context, Result, bail};
use std::ffi::CStr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct Pty {
    pub master: OwnedFd,
    pub child: Child,
}

pub fn spawn(mut command: Command) -> Result<Pty> {
    let master = open_master()?;
    let slave = open_slave(&master)?;

    let slave_fd = slave.as_raw_fd();
    command
        .stdin(clone_stdio(&slave)?)
        .stdout(clone_stdio(&slave)?)
        .stderr(clone_stdio(&slave)?);
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command
        .spawn()
        .context("could not launch claude — is it on your PATH?")?;
    drop(slave);
    Ok(Pty { master, child })
}

fn open_master() -> Result<OwnedFd> {
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        bail!("could not open a pseudo-terminal: {}", last_error());
    }
    let master = unsafe { OwnedFd::from_raw_fd(fd) };
    if unsafe { libc::grantpt(master.as_raw_fd()) } < 0 {
        bail!("grantpt failed: {}", last_error());
    }
    if unsafe { libc::unlockpt(master.as_raw_fd()) } < 0 {
        bail!("unlockpt failed: {}", last_error());
    }
    Ok(master)
}

fn open_slave(master: &OwnedFd) -> Result<OwnedFd> {
    let mut name = [0 as libc::c_char; 128];
    if unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) } != 0 {
        bail!("ptsname_r failed: {}", last_error());
    }
    let fd = unsafe { libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        let path = unsafe { CStr::from_ptr(name.as_ptr()) }.to_string_lossy();
        bail!("could not open {path}: {}", last_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn clone_stdio(fd: &OwnedFd) -> Result<Stdio> {
    Ok(fd.try_clone().context("could not duplicate the pty")?.into())
}

pub struct RawGuard {
    saved: libc::termios,
}

impl RawGuard {
    pub fn enable() -> Result<RawGuard> {
        let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } < 0 {
            bail!("could not read terminal attributes: {}", last_error());
        }

        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } < 0 {
            bail!("could not switch the terminal to raw mode: {}", last_error());
        }
        Ok(RawGuard { saved })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
    }
}

pub fn mirror_window_size(master: &OwnedFd) -> Result<()> {
    let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
    if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) } < 0 {
        bail!("could not read the window size: {}", last_error());
    }
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
        bail!("could not set the pty window size: {}", last_error());
    }
    Ok(())
}

/// Signals can't do real work from a handler, so the handler writes the signal
/// number into a pipe and the supervisor's poll loop reacts to it.
pub fn install_signal_pipe() -> Result<OwnedFd> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        bail!("could not create the signal pipe: {}", last_error());
    }
    let (read, write) = (fds[0], fds[1]);
    SIGNAL_PIPE_WRITE.store(write, std::sync::atomic::Ordering::SeqCst);

    for signal in [libc::SIGWINCH, libc::SIGTERM, libc::SIGHUP] {
        let handler = forward_signal as *const () as libc::sighandler_t;
        if unsafe { libc::signal(signal, handler) } == libc::SIG_ERR {
            bail!("could not install the signal handler: {}", last_error());
        }
    }
    Ok(unsafe { OwnedFd::from_raw_fd(read) })
}

static SIGNAL_PIPE_WRITE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn forward_signal(signal: libc::c_int) {
    let fd = SIGNAL_PIPE_WRITE.load(std::sync::atomic::Ordering::SeqCst);
    if fd >= 0 {
        let byte = signal as u8;
        unsafe { libc::write(fd, &byte as *const u8 as *const libc::c_void, 1) };
    }
}

pub fn is_terminal(fd: libc::c_int) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}
