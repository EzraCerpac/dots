use std::fs::File;
use std::io;
use std::os::fd::FromRawFd;
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use nix::libc;

use crate::support::{FixtureCommand, World};

fn check(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn foreground_terminal(command: &mut FixtureCommand<'_>) -> File {
    let mut size = libc::winsize {
        ws_row: 32,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut master, mut slave) = (-1, -1);
    // Keep the master open through the run, with just the slave on child stdin.
    unsafe {
        check(libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        ))
        .unwrap();
        let terminal = File::from_raw_fd(master);
        let stdin = File::from_raw_fd(slave);
        check(libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC)).unwrap();
        check(libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC)).unwrap();
        command.stdin(stdin).pre_exec(|| {
            check(libc::setsid())?;
            #[cfg(target_os = "macos")]
            let request = libc::c_ulong::from(libc::TIOCSCTTY);
            #[cfg(target_os = "linux")]
            let request = libc::TIOCSCTTY;
            check(libc::ioctl(0, request, 0))?;
            check(libc::tcsetpgrp(0, libc::getpgrp()))
        });
        terminal
    }
}

pub fn headless_session(command: &mut FixtureCommand<'_>) {
    // A redirected stdin alone can still leave a controlling terminal.
    command.stdin(Stdio::null());
    unsafe {
        command.pre_exec(|| check(libc::setsid()));
    }
}

#[derive(Debug)]
pub struct ProcessState {
    state: char,
    pub parent: libc::pid_t,
    pub group: libc::pid_t,
}

impl ProcessState {
    pub fn zombie(&self) -> bool {
        self.state == 'Z'
    }

    fn parse(fields: &str) -> Self {
        let mut fields = fields.split_whitespace();
        Self {
            state: fields.next().unwrap().chars().next().unwrap(),
            parent: fields.next().unwrap().parse().unwrap(),
            group: fields.next().unwrap().parse().unwrap(),
        }
    }
}

#[cfg(target_os = "macos")]
pub fn process_state(world: &World, pid: libc::pid_t) -> Option<ProcessState> {
    // Darwin proc_pidinfo can omit zombies while kernel ps still lists them.
    let result = world
        .command("/bin/ps")
        .args(["-o", "state=,ppid=,pgid=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    if result.status.code() == Some(1) && result.stdout.is_empty() && result.stderr.is_empty() {
        return None;
    }
    assert!(
        result.status.success(),
        "could not read fixture process state"
    );
    Some(ProcessState::parse(
        std::str::from_utf8(&result.stdout).unwrap(),
    ))
}

#[cfg(target_os = "linux")]
pub fn process_state(_world: &World, pid: libc::pid_t) -> Option<ProcessState> {
    // /proc is available on the Linux CI images, including BusyBox Alpine.
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return None;
        }
        Err(error) => panic!("could not read fixture process state: {error}"),
    };
    let (_, fields) = stat.rsplit_once(") ").expect("Linux process stat fields");
    Some(ProcessState::parse(fields))
}

// Peek without reaping. The fixture PID remains reserved while we clean up its session.
pub fn exit_success(pid: libc::pid_t) -> io::Result<Option<bool>> {
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        check(libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        ))?;
        Ok((info.si_pid() == pid)
            .then(|| info.si_code == libc::CLD_EXITED && info.si_status() == 0))
    }
}

pub fn wait_for_exit(pid: libc::pid_t, timeout: Duration) -> Option<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        match exit_success(pid) {
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            result => return result.ok().flatten(),
        }
    }
}

pub fn kill_owned_groups(session: libc::pid_t) {
    // The Go renderer may start a new group inside the fixture's session.
    // Only groups verified in that session are eligible for the fallback.
    unsafe {
        if session <= 1 || session == libc::getsid(0) {
            return;
        }
        let caller_group = libc::getpgrp();
        let mut signalled = std::collections::BTreeSet::new();
        for pid in process_ids() {
            if pid <= 1 || libc::getsid(pid) != session {
                continue;
            }
            let group = libc::getpgid(pid);
            if group > 1
                && group != caller_group
                && libc::getsid(pid) == session
                && signalled.insert(group)
            {
                libc::killpg(group, libc::SIGKILL);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn process_ids() -> Vec<libc::pid_t> {
    const PROC_ALL_PIDS: libc::c_uint = 1;
    unsafe {
        let size = libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0);
        if size <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0; size as usize / size_of::<libc::pid_t>() + 256];
        let bytes = libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr().cast(),
            (pids.len() * size_of::<libc::pid_t>()) as libc::c_int,
        );
        pids.truncate(bytes.max(0) as usize / size_of::<libc::pid_t>());
        pids
    }
}

#[cfg(target_os = "linux")]
fn process_ids() -> Vec<libc::pid_t> {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect()
}
