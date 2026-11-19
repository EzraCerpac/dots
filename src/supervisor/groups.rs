use nix::libc;
use std::io;

// The caller keeps the group leader's PID reserved throughout this query.
#[cfg(target_os = "macos")]
pub(super) fn has_live(group: libc::pid_t, observer: Option<libc::pid_t>) -> io::Result<bool> {
    any_member(group, observer, false, |_| Ok(true))
}

// The detached group's original leader is known to have exited and is still
// waitable. It need not appear alive to keep the group's identity reserved.
#[cfg(target_os = "macos")]
pub(super) fn has_live_after_leader_exit(leader: libc::pid_t) -> io::Result<bool> {
    any_member(leader, Some(leader), true, |_| Ok(true))
}

#[cfg(target_os = "macos")]
pub(super) fn has_stopped_child(group: libc::pid_t, leader: libc::pid_t) -> io::Result<bool> {
    const PROC_FLAG_TRACED: u32 = 2; // sys/proc_info.h
    any_member(group, Some(leader), false, |pid| {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::uninit();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let count = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if count == size {
            let info = unsafe { info.assume_init() };
            return Ok(info.pbi_status == libc::SSTOP && info.pbi_flags & PROC_FLAG_TRACED == 0);
        }
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) {
            return Ok(false);
        }
        Err(error)
    })
}

#[cfg(target_os = "macos")]
fn any_member(
    group: libc::pid_t,
    exclude: Option<libc::pid_t>,
    strict: bool,
    mut matches: impl FnMut(libc::pid_t) -> io::Result<bool>,
) -> io::Result<bool> {
    let mut pids = vec![0 as libc::pid_t; 32];
    loop {
        let count = unsafe {
            *libc::__error() = 0;
            let count = libc::proc_listpgrppids(
                group,
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(pids.as_slice()) as libc::c_int,
            );
            if *libc::__error() != 0 || count < 0 {
                return Err(io::Error::last_os_error());
            }
            count as usize
        };
        if members_match(&pids[..count], exclude, strict, |pid| {
            if process_group(pid)? == Some(group) {
                return matches(pid).map(Some);
            }
            Ok(None)
        })? {
            return Ok(true);
        }
        if count < pids.len() {
            return Ok(false);
        }
        if pids.len() >= 4096 {
            return Err(io::Error::other("process-group listing was truncated"));
        }
        pids.resize(pids.len() * 2, 0);
    }
}

#[cfg(target_os = "macos")]
fn members_match(
    pids: &[libc::pid_t],
    exclude: Option<libc::pid_t>,
    strict: bool,
    mut inspect: impl FnMut(libc::pid_t) -> io::Result<Option<bool>>,
) -> io::Result<bool> {
    let mut ambiguous = false;
    for &pid in pids.iter().filter(|&&pid| Some(pid) != exclude) {
        match inspect(pid)? {
            Some(true) => return Ok(true),
            Some(false) => {}
            // A listed process might have forked a replacement before exiting.
            None => ambiguous |= strict,
        }
    }
    if ambiguous {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "process group changed during inspection",
        ));
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
pub(super) fn has_live_after_leader_exit(leader: libc::pid_t) -> io::Result<bool> {
    has_live(leader, Some(leader))
}

#[cfg(target_os = "linux")]
pub(super) fn has_live(group: libc::pid_t, observer: Option<libc::pid_t>) -> io::Result<bool> {
    any_member(group, observer, |state| {
        !matches!(state, b'Z' | b'X' | b'x')
    })
}

#[cfg(target_os = "linux")]
pub(super) fn has_stopped_child(group: libc::pid_t, leader: libc::pid_t) -> io::Result<bool> {
    any_member(group, Some(leader), |state| state == b'T')
}

#[cfg(target_os = "linux")]
fn any_member(
    group: libc::pid_t,
    exclude: Option<libc::pid_t>,
    mut matches: impl FnMut(u8) -> bool,
) -> io::Result<bool> {
    use std::fs;

    for item in fs::read_dir("/proc")? {
        let item = item?;
        let Some(pid) = item
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if Some(pid) == exclude || process_group(pid)? != Some(group) {
            continue;
        }
        let stat = match fs::read(item.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        // The command name can contain spaces or closing parentheses.
        let state = stat
            .iter()
            .rposition(|byte| *byte == b')')
            .and_then(|end| stat.get(end + 2));
        match state {
            Some(&state) if matches(state) => return Ok(true),
            Some(_) => {}
            None => {
                return Err(io::Error::other(
                    "could not read a group member's process state",
                ));
            }
        }
    }
    Ok(false)
}

fn process_group(pid: libc::pid_t) -> io::Result<Option<libc::pid_t>> {
    let group = unsafe { libc::getpgid(pid) };
    if group != -1 {
        return Ok(Some(group));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        // Darwin also returns ESRCH for an unreaped zombie.
        return Ok(None);
    }
    Err(error)
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn a_vanished_listed_member_requires_another_snapshot() {
    let leader = 40;
    let stale = members_match(&[leader, 41], Some(leader), true, |_| Ok(None));
    assert_eq!(stale.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    let replacement = members_match(&[leader, 42], Some(leader), true, |pid| {
        assert_eq!(pid, 42);
        Ok(Some(true))
    });
    assert!(replacement.unwrap());
    let empty = members_match(&[leader], Some(leader), true, |_| unreachable!());
    assert!(!empty.unwrap());
}
