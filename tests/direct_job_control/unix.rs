use nix::libc;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::time::{Duration, Instant};

pub type Pid = libc::pid_t;
pub const TIMEOUT: Duration = Duration::from_secs(10);

pub fn wait_for<T>(what: &str, ready: impl FnMut() -> Option<T>) -> T {
    wait_for_with(what, ready, String::new)
}

pub fn wait_for_with<T>(
    what: &str,
    mut ready: impl FnMut() -> Option<T>,
    details: impl FnOnce() -> String,
) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what} {}",
            details()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn check(value: libc::c_int) -> io::Result<()> {
    if value == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn open_pty() -> (File, File) {
    let (mut master, mut slave) = (-1, -1);
    // openpty creates a fresh pair. Files own the descriptors from here on.
    unsafe {
        check(libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ))
        .unwrap();
        let pair = (File::from_raw_fd(master), File::from_raw_fd(slave));
        for fd in [master, slave] {
            check(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC)).unwrap();
        }
        pair
    }
}

pub fn nonblocking(fd: &impl AsRawFd) {
    let fd = fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        check(flags).unwrap();
        check(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK)).unwrap();
    }
}

pub fn termios(fd: RawFd) -> libc::termios {
    let mut modes = std::mem::MaybeUninit::uninit();
    unsafe {
        check(libc::tcgetattr(fd, modes.as_mut_ptr())).unwrap();
        modes.assume_init()
    }
}

pub fn set_termios(fd: RawFd, modes: &libc::termios) {
    unsafe { check(libc::tcsetattr(fd, libc::TCSANOW, modes)) }.unwrap();
}

pub fn tty_name(fd: RawFd) -> Option<String> {
    let mut buffer = [0; 1024];
    let result = unsafe { libc::ttyname_r(fd, buffer.as_mut_ptr(), buffer.len()) };
    (result == 0).then(|| {
        unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned()
    })
}

pub fn foreground(fd: RawFd) -> Pid {
    unsafe { libc::tcgetpgrp(fd) }
}

/// Safety: call in the new child after setsid and before exec.
pub unsafe fn acquire_controlling_tty() -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let request = libc::c_ulong::from(libc::TIOCSCTTY);
    #[cfg(target_os = "linux")]
    let request = libc::TIOCSCTTY;
    unsafe { check(libc::ioctl(0, request, 0)) }
}

pub fn session(pid: Pid) -> Pid {
    unsafe { libc::getsid(pid) }
}

pub fn group(pid: Pid) -> Pid {
    unsafe { libc::getpgid(pid) }
}

#[cfg(target_os = "linux")]
pub fn process_name(pid: Pid) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .unwrap_or_default()
        .trim_end()
        .to_owned()
}

#[cfg(target_os = "macos")]
pub fn process_name(pid: Pid) -> String {
    let mut bytes = [0; 256];
    let len = unsafe { libc::proc_name(pid, bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf8_lossy(&bytes[..len as usize])
        .trim_end_matches('\0')
        .to_owned()
}

pub fn signal(pid: Pid, signal: libc::c_int) -> io::Result<()> {
    assert!(pid > 1);
    unsafe { check(libc::kill(pid, signal)) }
}

#[cfg(target_os = "linux")]
pub fn processes() -> Vec<Pid> {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}

#[cfg(target_os = "macos")]
pub fn processes() -> Vec<Pid> {
    const PROC_ALL_PIDS: libc::c_uint = 1; // <sys/proc_info.h>
    unsafe {
        let capacity = libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0);
        assert!(capacity > 0);
        let mut pids = vec![0; capacity as usize / size_of::<Pid>() + 256];
        let bytes = libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<Pid>()) as libc::c_int,
        );
        assert!(bytes > 0);
        pids.retain(|pid| *pid > 1);
        pids
    }
}

#[cfg(target_os = "linux")]
pub fn stopped(pid: Pid) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")
                .map(|(_, fields)| fields.starts_with('T'))
        })
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub fn stopped(pid: Pid) -> bool {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        ) == size
            && info.assume_init().pbi_status == libc::SSTOP
    }
}
