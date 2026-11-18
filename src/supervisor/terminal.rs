use nix::libc;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::pipe::{nonblocking, ready};

const OBSERVER: &str = "__dots_foreground_signal_observer";
const OBSERVER_ENV: &str = "DOTS_INTERNAL_SIGNAL_OBSERVER";
const INTERRUPT: u8 = b'I';
const READY: u8 = b'R';
const FENCE: u8 = b'F';

/// Internal re-exec path. All communication uses private pipes.
pub fn terminal_observer(mut args: impl Iterator<Item = OsString>) -> Option<u8> {
    args.next()?;
    if args.next()? != OBSERVER
        || args.next().is_some()
        || std::env::var_os(OBSERVER_ENV).as_deref() != Some(OsStr::new("1"))
    {
        return None;
    }
    Some(observe().map_or(1, |()| 0))
}

extern "C" fn interrupt(_: libc::c_int) {
    // One byte is atomic on the private pipe. A full pipe already has notices.
    unsafe {
        #[cfg(target_os = "macos")]
        let errno = libc::__error();
        #[cfg(target_os = "linux")]
        let errno = libc::__errno_location();
        let previous = *errno;
        libc::write(libc::STDOUT_FILENO, (&INTERRUPT as *const u8).cast(), 1);
        *errno = previous;
    }
}

fn observe() -> io::Result<()> {
    // This runs after exec, with no shared Rust state from the parent.
    for (signal, handler) in [
        (libc::SIGINT, interrupt as *const () as libc::sighandler_t),
        (libc::SIGQUIT, interrupt as *const () as libc::sighandler_t),
        (libc::SIGTSTP, libc::SIG_IGN),
        (libc::SIGTTIN, libc::SIG_IGN),
        (libc::SIGTTOU, libc::SIG_IGN),
    ] {
        if unsafe { libc::signal(signal, handler) } == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    nonblocking(libc::STDOUT_FILENO)?;
    let mut stdout = io::stdout().lock();
    stdout.write_all(&[READY])?;
    stdout.flush()?;
    let mut stdin = io::stdin().lock();
    let mut marker = [0; 1];
    while stdin.read(&mut marker)? != 0 {
        if marker == [FENCE] {
            stdout.write_all(&[FENCE])?;
            stdout.flush()?;
        }
    }
    Ok(())
}

pub(super) struct TerminalJob {
    tty: File,
    caller: libc::pid_t,
    modes: libc::termios,
    observer: Child,
    handed_off: bool,
    fence_sent: bool,
    fenced: bool,
}

impl TerminalJob {
    pub(super) fn start() -> io::Result<Option<Self>> {
        let Some((tty, caller)) = foreground_terminal() else {
            return Ok(None);
        };
        let mut modes = std::mem::MaybeUninit::uninit();
        if unsafe { libc::tcgetattr(tty.as_raw_fd(), modes.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let observer = Command::new(std::env::current_exe()?)
            .arg(OBSERVER)
            .env(OBSERVER_ENV, "1")
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut job = Self {
            tty,
            caller,
            // tcgetattr initialized the entire termios value.
            modes: unsafe { modes.assume_init() },
            observer,
            handed_off: false,
            fence_sent: false,
            fenced: false,
        };
        let output = job.observer.stdout.as_mut().unwrap();
        let mut hello = [0];
        if !ready(output.as_raw_fd(), libc::POLLIN, Duration::from_secs(1))?
            || output.read_exact(&mut hello).is_err()
            || hello != [READY]
        {
            return Err(io::Error::other("foreground signal observer did not start"));
        }
        nonblocking(output.as_raw_fd())?;
        Ok(Some(job))
    }

    pub(super) fn group(&self) -> libc::pid_t {
        self.observer.id() as libc::pid_t
    }

    pub(super) fn prepare(&mut self, command: &mut Command) {
        let fd = self.tty.as_raw_fd();
        let group = self.group();
        // Mark the guard before spawn so even an exec failure restores pgrp.
        self.handed_off = true;
        command.process_group(group);
        unsafe {
            command.pre_exec(move || {
                // Reset the supervisor's handler before transferring foreground.
                for signal in [libc::SIGINT, libc::SIGQUIT, libc::SIGTERM, libc::SIGHUP] {
                    if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                        return Err(io::Error::last_os_error());
                    }
                }
                foreground(fd, group)
            });
        }
    }

    pub(super) fn notices(&mut self) -> io::Result<bool> {
        let mut interrupted = false;
        let output = self.observer.stdout.as_mut().unwrap();
        let mut buffer = [0; 256];
        loop {
            match output.read(&mut buffer) {
                Ok(0) => {
                    self.fenced = true;
                    return Ok(interrupted);
                }
                Ok(n) => {
                    interrupted |= buffer[..n].contains(&INTERRUPT);
                    self.fenced |= buffer[..n].contains(&FENCE);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(interrupted),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Reclaim before asking the observer to acknowledge its pending signals.
    pub(super) fn fence(&mut self) -> io::Result<bool> {
        if !self.fence_sent {
            self.reclaim()?;
            self.fence_sent = true;
            if let Err(error) = self.observer.stdin.as_mut().unwrap().write_all(&[FENCE]) {
                if error.kind() != io::ErrorKind::BrokenPipe {
                    return Err(error);
                }
                self.fenced = true;
            }
        }
        Ok(self.fenced)
    }

    pub(super) fn suspend(&mut self) -> io::Result<()> {
        let fd = self.tty.as_raw_fd();
        let mut stopped_modes = std::mem::MaybeUninit::uninit();
        if unsafe { libc::tcgetattr(fd, stopped_modes.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error());
        }
        self.reclaim()?;
        // Use an unconditional stop so a shell can resume dots with fg/bg.
        if unsafe { libc::raise(libc::SIGSTOP) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::tcgetpgrp(fd) } == self.caller {
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, stopped_modes.as_ptr()) } == -1 {
                return Err(io::Error::last_os_error());
            }
            if let Err(error) = foreground(fd, self.group()) {
                unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.modes) };
                return Err(error);
            }
            self.handed_off = true;
        }
        if unsafe { libc::kill(-self.group(), libc::SIGCONT) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }

    fn reclaim(&mut self) -> io::Result<()> {
        if !self.handed_off {
            return Ok(());
        }
        let fd = self.tty.as_raw_fd();
        if unsafe { libc::tcgetpgrp(fd) } == self.group() {
            foreground(fd, self.caller)?;
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &self.modes) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        self.handed_off = false;
        Ok(())
    }
}

impl Drop for TerminalJob {
    fn drop(&mut self) {
        let _ = self.reclaim();
        let _ = self.observer.kill();
        let _ = self.observer.wait();
    }
}

pub(super) fn foreground_terminal() -> Option<(File, libc::pid_t)> {
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let caller = unsafe { libc::getpgrp() };
    (unsafe { libc::tcgetpgrp(tty.as_raw_fd()) } == caller).then_some((tty, caller))
}

// Also called from pre_exec. Keep this to async-signal-safe libc calls.
fn foreground(fd: RawFd, group: libc::pid_t) -> io::Result<()> {
    unsafe {
        let mut blocked = std::mem::zeroed();
        let mut previous = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        if libc::sigprocmask(libc::SIG_BLOCK, &blocked, &mut previous) == -1 {
            return Err(io::Error::last_os_error());
        }
        let result = if libc::tcsetpgrp(fd, group) == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
        let restored = libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if restored == -1 && result.is_ok() {
            return Err(io::Error::last_os_error());
        }
        result
    }
}
