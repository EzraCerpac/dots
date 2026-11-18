use std::io::{self, Read as _};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd as _, RawFd};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use nix::libc;

pub(super) struct Budget {
    deadline: Instant,
    cancelled: AtomicBool,
    groups: Mutex<Vec<u32>>,
}

impl Budget {
    pub(super) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            cancelled: AtomicBool::new(false),
            groups: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn expired(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || self.remaining().is_zero()
    }

    pub(super) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn register(&self, pid: u32) -> bool {
        let mut groups = self.groups.lock().unwrap();
        if self.expired() {
            return false;
        }
        groups.push(pid);
        true
    }

    pub(super) fn cancel(&self) {
        let groups = self.groups.lock().unwrap();
        self.cancelled.store(true, Ordering::Release);
        for &pid in &*groups {
            kill_group(pid);
        }
    }

    fn retire(&self, pid: u32) {
        let mut groups = self.groups.lock().unwrap();
        // The owner has not waited on the child. Signal before unregistering,
        // and unregister before allowing the leader's PID to be reused.
        kill_group(pid);
        groups.retain(|id| *id != pid);
    }
}

struct ChildGroup<'a> {
    child: Child,
    budget: &'a Budget,
    reaped: bool,
}

impl<'a> ChildGroup<'a> {
    fn spawn(command: &mut Command, budget: &'a Budget) -> Option<Self> {
        if budget.expired() || !leaders_waitable() {
            return None;
        }
        let child = command
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let group = Self {
            child,
            budget,
            reaped: false,
        };
        if budget.register(group.child.id()) {
            Some(group)
        } else {
            // Drop signals and reaps even when spawning straddled cancellation.
            None
        }
    }

    fn reap(&mut self) -> io::Result<ExitStatus> {
        self.budget.retire(self.child.id());
        self.reaped = true;
        self.child.wait()
    }
}

impl Drop for ChildGroup<'_> {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.reap();
        }
    }
}

pub(super) fn leaders_waitable() -> bool {
    sigchld_action().is_some_and(|action| {
        action.sa_sigaction == libc::SIG_DFL && action.sa_flags & libc::SA_NOCLDWAIT == 0
    })
}

fn sigchld_action() -> Option<libc::sigaction> {
    let mut action = MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: A null input queries the current disposition without changing it.
    let result = unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) };
    // SAFETY: A successful query initializes the output.
    (result == 0).then(|| unsafe { action.assume_init() })
}

fn kill_group(pid: u32) {
    // SAFETY: process_group(0) and the unreaped child reserve this group ID.
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
}

fn observe_exit(pid: u32) -> io::Result<()> {
    loop {
        let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: info is writable, and WNOWAIT leaves this child unreaped.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd is borrowed from live ChildStdout for both fcntl calls.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn run(command: &mut Command, budget: &Budget, limit: usize) -> Option<Vec<u8>> {
    let mut group = ChildGroup::spawn(command, budget)?;
    let mut stdout = group.child.stdout.take().expect("piped stdout");
    nonblocking(stdout.as_raw_fd()).ok()?;
    let mut output = Vec::new();
    let mut chunk = [0; 8192];
    while !budget.expired() {
        match stdout.read(&mut chunk) {
            Ok(0) => {
                // stdout EOF can precede leader exit. The coordinator can
                // still signal the group while this worker observes exit.
                observe_exit(group.child.id()).ok()?;
                let status = group.reap().ok()?;
                return (status.success() && !budget.expired()).then_some(output);
            }
            Ok(count) if output.len() + count <= limit => output.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(budget.remaining().min(Duration::from_millis(4)));
            }
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::tests::isolated_env;

    fn set_sigchld(handler: libc::sighandler_t, flags: libc::c_int) -> io::Result<()> {
        // SAFETY: The action starts zeroed, with an empty mask and no restorer.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler;
        action.sa_flags = flags;
        // SAFETY: The action is initialized and only the isolated test process calls this.
        if unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[test]
    fn screening_declines_children_when_sigchld_can_auto_reap() {
        const MODE: &str = "DOTS_TEST_RECEIPT_SIGCHLD";
        if let Ok(mode) = std::env::var(MODE) {
            let (handler, flags) = match mode.as_str() {
                "ignored" => (libc::SIG_IGN, 0),
                "no-cld-wait" => {
                    // Linux clears action flags during exec. Set this mode
                    // in the re-executed fixture before calling receipt code.
                    set_sigchld(libc::SIG_DFL, libc::SA_NOCLDWAIT).unwrap();
                    (libc::SIG_DFL, libc::SA_NOCLDWAIT)
                }
                _ => unreachable!(),
            };
            let disposition = || {
                let action = sigchld_action().unwrap();
                (action.sa_sigaction, action.sa_flags & libc::SA_NOCLDWAIT)
            };
            assert_eq!(disposition(), (handler, flags));
            assert!(!leaders_waitable());
            let budget = Budget::new(Instant::now() + Duration::from_secs(15));
            let mut command = Command::new("/bin/sh");
            command
                .env_clear()
                .envs(&isolated_env(&std::env::current_dir().unwrap()).vars)
                .args(["-c", "exec /bin/sleep 60"]);
            assert!(ChildGroup::spawn(&mut command, &budget).is_none());
            assert_eq!(disposition(), (handler, flags));
            return;
        }

        let fixture = tempfile::tempdir().unwrap();
        let env = isolated_env(fixture.path());
        std::fs::create_dir(fixture.path().join("home")).unwrap();
        for mode in ["ignored", "no-cld-wait"] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .env_clear()
                .envs(&env.vars)
                .env(MODE, mode)
                .current_dir(fixture.path())
                .args([
                    "--exact",
                    "receipt::child::tests::screening_declines_children_when_sigchld_can_auto_reap",
                ]);
            if mode == "ignored" {
                // SAFETY: The pre-exec hook only uses sigaction and reads errno on failure.
                unsafe { command.pre_exec(|| set_sigchld(libc::SIG_IGN, 0)) };
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "{mode}: {}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
    }

    #[test]
    fn cancellation_signals_an_exited_leaders_group_before_reaping() {
        let fixture = tempfile::tempdir().unwrap();
        let env = isolated_env(fixture.path());
        std::fs::create_dir(fixture.path().join("home")).unwrap();
        let budget = Budget::new(Instant::now() + Duration::from_secs(15));
        let mut command = Command::new("/bin/sh");
        command
            .env_clear()
            .envs(&env.vars)
            .current_dir(fixture.path())
            .stdin(Stdio::null())
            .args(["-c", "/bin/sleep 60 &"]);
        let mut group = ChildGroup::spawn(&mut command, &budget).unwrap();
        let pid = group.child.id();
        let mut stdout = group.child.stdout.take().unwrap();

        // The leader has exited and the descendant still holds the pipe.
        observe_exit(pid).unwrap();
        nonblocking(stdout.as_raw_fd()).unwrap();
        assert_eq!(
            stdout.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );

        budget.cancel();
        // This succeeds only while the exited leader remains our unreaped
        // child. The group signal was issued while that PID was reserved.
        observe_exit(pid).unwrap();
        let mut pipe = libc::pollfd {
            fd: stdout.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        // SAFETY: pipe contains the live stdout FD and is writable.
        assert_eq!(unsafe { libc::poll(&mut pipe, 1, 3_000) }, 1);
        assert_eq!(stdout.read(&mut [0]).unwrap(), 0);
        assert_eq!(group.reap().unwrap().code(), Some(0));
        assert!(budget.groups.lock().unwrap().is_empty());
    }
}
