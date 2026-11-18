use nix::libc;
use std::io;

// The caller keeps the group leader's PID reserved throughout this query.
#[cfg(target_os = "macos")]
pub(super) fn has_live(group: libc::pid_t, observer: Option<libc::pid_t>) -> io::Result<bool> {
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
        for &pid in &pids[..count] {
            if Some(pid) != observer && process_group(pid)? == Some(group) {
                return Ok(true);
            }
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

#[cfg(target_os = "linux")]
pub(super) fn has_live(group: libc::pid_t, observer: Option<libc::pid_t>) -> io::Result<bool> {
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
        if Some(pid) == observer || process_group(pid)? != Some(group) {
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
            Some(b'Z' | b'X' | b'x') => {}
            Some(_) => return Ok(true),
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
