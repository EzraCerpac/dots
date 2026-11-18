//! Native stage and probe lifecycle, plus the plain and verbose up presentation.
mod logs;
mod pipe;
mod signals;
mod terminal;
#[cfg(test)]
mod tests;

use nix::libc;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::report::{Command as ReportCommand, Issue, StageId};
use crate::run::{Policy, exit_code};
use crate::{Ctx, Env};
use logs::Logs;
use pipe::Pipes;
use signals::Signals;
use terminal::{TerminalJob, foreground_terminal};

pub(crate) use logs::remove_run_logs;
pub use terminal::terminal_observer;

const TICK: Duration = Duration::from_millis(10);
const TERM_AFTER: Duration = Duration::from_millis(150);
const KILL_AFTER: Duration = Duration::from_millis(650);
const DRAIN_IDLE: Duration = Duration::from_secs(1);
const DRAIN_AFTER_KILL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Direct,
    Fragment,
    Up { verbose: bool, native_prompts: bool },
}

impl Mode {
    pub(crate) fn up(ctx: &Ctx) -> Self {
        Self::Up {
            verbose: ctx.flags.verbose,
            native_prompts: !ctx.flags.plain
                && !ctx.flags.verbose
                && ctx.env.get("CI").is_none_or(|value| value.is_empty())
                && io::stdout().is_terminal()
                && foreground_terminal().is_some(),
        }
    }
}

#[derive(Clone, Copy)]
enum Placement {
    Foreground,
    Background,
    Detached,
    #[cfg(test)]
    HeadlessForeground,
}

impl Placement {
    fn foreground(self) -> bool {
        match self {
            Self::Foreground => true,
            #[cfg(test)]
            Self::HeadlessForeground => true,
            _ => false,
        }
    }
}

pub(crate) struct Supervisor {
    mode: Mode,
    signals: io::Result<Signals>,
    state: Mutex<State>,
    idle: Condvar,
    log_base: io::Result<PathBuf>,
    logs: Mutex<Logs>,
}

#[derive(Default)]
struct State {
    children: Vec<Entry>,
    last_signal: libc::c_int,
    cancel: Option<Cancellation>,
    closed: bool,
}

#[derive(Clone, Copy)]
struct Entry {
    pid: libc::pid_t,
    target: libc::pid_t,
    foreground: bool,
}

struct Cancellation {
    start: Instant,
    // TTY line discipline already delivered the interrupt to this group. While
    // its leader still runs, let it decide how to handle the keystroke.
    terminal_pid: Option<libc::pid_t>,
    foreground: Option<libc::pid_t>,
    status: Option<u8>,
    escalation: Escalation,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Escalation {
    #[default]
    Interrupt,
    Terminate,
    Kill,
}

#[derive(Clone, Copy)]
pub(crate) struct Completion {
    pub(crate) status: u8,
    pub(crate) cancelled: bool,
}

impl Supervisor {
    pub(crate) fn new(env: &Env, mode: Mode) -> Self {
        let signals = Signals::install();
        let last_signal = signals.as_ref().map_or(0, |signals| signals.start);
        Self {
            mode,
            signals,
            state: Mutex::new(State {
                last_signal,
                ..State::default()
            }),
            idle: Condvar::new(),
            log_base: env.state_dir(),
            logs: Mutex::new(Logs::default()),
        }
    }

    pub(crate) fn admit_stage(&self, add: impl FnOnce() -> StageId) -> Option<StageId> {
        let state = self.state();
        if state.closed || state.cancel.is_some() {
            return None;
        }
        Some(add())
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.state().cancel.is_some()
    }

    pub(crate) fn run(
        &self,
        id: StageId,
        alongside: bool,
        mut command: Command,
        policy: Policy,
    ) -> io::Result<ExitStatus> {
        let up = matches!(self.mode, Mode::Up { .. });
        let native_prompt = !alongside
            && policy == Policy::Interactive
            && matches!(
                self.mode,
                Mode::Up {
                    native_prompts: true,
                    ..
                }
            );
        let capture = alongside || (up && !native_prompt);
        command.stdin(if alongside {
            Stdio::null()
        } else {
            Stdio::inherit()
        });
        self.spawn_streams(&mut command, capture);

        let placement = if alongside {
            Placement::Detached
        } else {
            Placement::Foreground
        };
        let mut running = self.spawn(command, placement)?;
        if !capture {
            let status = running.wait()?;
            if native_prompt && !status.success() {
                self.logs
                    .lock()
                    .unwrap()
                    .failed(id, Some(exit_code(status)));
            }
            return Ok(status);
        }
        let mut diagnostic = if up {
            self.logs.lock().unwrap().stage(&self.log_base, id)
        } else {
            logs::Diagnostic::default()
        };
        let verbose = matches!(self.mode, Mode::Up { verbose: true, .. }) && !alongside;
        let result = Pipes::take(&mut running.child)?.pump(&mut running, verbose, |_, bytes| {
            if up {
                diagnostic.record(bytes);
            }
        });
        if !up {
            return result;
        }
        let mut logs = self.logs.lock().unwrap();
        if result.as_ref().is_ok_and(|status| !status.success()) {
            logs.failed(id, None);
        }
        logs.store(id, diagnostic);
        result
    }

    pub(crate) fn probe(
        &self,
        mut command: Command,
        input: Stdio,
        alongside: bool,
    ) -> io::Result<Output> {
        command.stdin(input);
        self.spawn_streams(&mut command, true);
        let placement = if alongside {
            Placement::Detached
        } else {
            Placement::Background
        };
        let mut running = self.spawn(command, placement)?;
        Pipes::take(&mut running.child)?.collect(&mut running)
    }

    fn spawn_streams(&self, command: &mut Command, capture: bool) {
        let stream = || {
            if capture {
                Stdio::piped()
            } else {
                Stdio::inherit()
            }
        };
        command.stdout(stream()).stderr(stream());
    }

    fn spawn(&self, mut command: Command, placement: Placement) -> io::Result<Running<'_>> {
        if let Err(error) = &self.signals {
            return Err(io::Error::other(format!(
                "install cancellation handlers: {error}"
            )));
        }
        let mut state = self.state();
        if state.closed || state.cancel.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "dots run was cancelled",
            ));
        }
        if placement.foreground() && state.children.iter().any(|child| child.foreground) {
            return Err(io::Error::other(
                "another foreground stage is already running",
            ));
        }
        let fragment = matches!(self.mode, Mode::Fragment);
        let mut terminal = if matches!(placement, Placement::Foreground) && !fragment {
            TerminalJob::start()?
        } else {
            None
        };
        if let Some(terminal) = &mut terminal {
            terminal.prepare(&mut command);
        } else if !fragment {
            if matches!(placement, Placement::Detached) {
                unsafe {
                    // setsid makes a new group as well as a new session.
                    command.pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            } else {
                command.process_group(0);
            }
        }
        state.refresh_signals();
        if state.cancel.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "dots run was cancelled",
            ));
        }
        let child = command.spawn()?;
        drop(command);
        let pid = child.id() as libc::pid_t;
        let target = if fragment {
            // A bootstrap callback inherits the outer mise stage's group.
            pid
        } else {
            -terminal.as_ref().map_or(pid, TerminalJob::group)
        };
        let entry = Entry {
            pid,
            target,
            foreground: placement.foreground(),
        };
        state.children.push(entry);
        state.refresh_signals();
        Ok(Running {
            supervisor: self,
            child,
            terminal,
            entry,
            leader_exit: None,
            last_output: Instant::now(),
            group_cleanup: None,
            draining: false,
            leader_kill: None,
            reaped: false,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.refresh_signals();
        state
    }

    pub(crate) fn complete(&self, command: Option<ReportCommand>, status: u8) -> Completion {
        let mut state = self.state();
        state.closed = true;
        while !state.children.is_empty() {
            state = self
                .idle
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        state.refresh_signals();
        match &state.cancel {
            Some(cancel) => Completion {
                status: if command == Some(ReportCommand::Sync) {
                    0
                } else {
                    cancel.status.unwrap_or(130)
                },
                cancelled: true,
            },
            None => Completion {
                status,
                cancelled: false,
            },
        }
    }

    pub(crate) fn enrich_issue(&self, id: StageId, issue: &mut Issue) {
        self.logs.lock().unwrap().enrich(id, issue);
    }

    pub(crate) fn replay_alongside(&self, id: StageId, output: &mut impl Write) -> io::Result<()> {
        if matches!(self.mode, Mode::Up { verbose: true, .. }) {
            let replay = self.logs.lock().unwrap().replay(id);
            if let Some((path, needs_newline)) = replay {
                let mut log = File::open(path)?;
                if io::copy(&mut log, output)? != 0 && needs_newline {
                    output.write_all(b"\n")?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn log_id(&self) -> Option<String> {
        self.logs.lock().unwrap().id()
    }

    pub(crate) fn keep_logs(&self) {
        self.logs.lock().unwrap().keep();
    }
}

impl State {
    fn refresh_signals(&mut self) {
        if let Some(signal) = Signals::pending(&mut self.last_signal) {
            self.cancel(Some(signal), None);
        }
        let Some(cancel) = &mut self.cancel else {
            return;
        };
        for (delay, escalation, signal) in [
            (TERM_AFTER, Escalation::Terminate, libc::SIGTERM),
            (KILL_AFTER, Escalation::Kill, libc::SIGKILL),
        ] {
            if cancel.start.elapsed() >= delay
                && cancel.escalation != Escalation::Kill
                && cancel.escalation != escalation
            {
                cancel.escalation = escalation;
                for entry in &self.children {
                    if Some(entry.pid) != cancel.terminal_pid {
                        signal_target(entry.target, signal);
                    }
                }
            }
        }
    }

    fn cancel(&mut self, signal: Option<libc::c_int>, terminal_pid: Option<libc::pid_t>) {
        if let Some(cancel) = &mut self.cancel {
            if let Some(signal) = signal
                && let Some(pid) = cancel.terminal_pid.take()
                && let Some(entry) = self.children.iter().find(|entry| entry.pid == pid)
            {
                // An explicit signal to dots supersedes a terminal-only cancel.
                signal_target(entry.target, signal);
                cancel.start = Instant::now();
                cancel.escalation = Escalation::Interrupt;
            }
            return;
        }
        let foreground = self
            .children
            .iter()
            .find(|entry| entry.foreground)
            .and_then(|entry| {
                (Some(entry.pid) == terminal_pid
                    || matches!(observe(entry.pid, false), Ok(Observed::Running)))
                .then_some(entry.pid)
            });
        self.cancel = Some(Cancellation {
            start: Instant::now(),
            terminal_pid,
            foreground,
            status: None,
            escalation: Escalation::Interrupt,
        });
        for entry in &self.children {
            if Some(entry.pid) != terminal_pid {
                signal_target(entry.target, signal.unwrap_or(libc::SIGINT));
            }
        }
    }

    fn terminal_interrupt(&mut self, pid: libc::pid_t, during_drain: bool) {
        if self.cancel.is_some() {
            return;
        }
        self.cancel(None, Some(pid));
        if during_drain && let Some(cancel) = &mut self.cancel {
            // The kernel already signalled that group, but the recorded
            // leader had exited. Escalate against remaining group members.
            cancel.foreground = None;
            cancel.terminal_pid = None;
        }
    }
}

struct Running<'a> {
    supervisor: &'a Supervisor,
    child: Child,
    terminal: Option<TerminalJob>,
    entry: Entry,
    leader_exit: Option<Instant>,
    last_output: Instant,
    group_cleanup: Option<Instant>,
    draining: bool,
    leader_kill: Option<bool>,
    reaped: bool,
}

impl Running<'_> {
    fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.poll(true, false)? {
                return Ok(status);
            }
            std::thread::sleep(TICK);
        }
    }

    fn poll(&mut self, drained: bool, display_pending: bool) -> io::Result<Option<ExitStatus>> {
        if let Some(job) = &mut self.terminal
            && job.notices()?
        {
            self.supervisor
                .state()
                .terminal_interrupt(self.entry.pid, self.draining);
        }

        let mut state = self.supervisor.state();
        let observation = observe(self.entry.pid, self.terminal.is_some())?;
        if !matches!(observation, Observed::Exited { .. })
            && self.leader_kill.is_none()
            && state.cancel.as_ref().is_some_and(|cancel| {
                cancel.escalation == Escalation::Kill && cancel.terminal_pid != Some(self.entry.pid)
            })
        {
            // A group signal can succeed for its observer or a descendant
            // even when the native leader refuses it.
            let signalled = signal_target(self.entry.pid, libc::SIGKILL);
            self.leader_kill = Some(signalled);
            if !signalled {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "could not stop the interrupted child",
                ));
            }
        }
        match observation {
            Observed::Running => return Ok(None),
            Observed::Stopped if state.cancel.is_none() => {
                drop(state);
                if acknowledge_stop(self.entry.pid)?
                    && let Some(terminal) = &mut self.terminal
                {
                    terminal.suspend()?;
                }
                return Ok(None);
            }
            Observed::Stopped => {
                let _ = acknowledge_stop(self.entry.pid);
                return Ok(None);
            }
            Observed::Exited { signal } => {
                if state.cancel.is_none() && signal == Some(libc::SIGINT) {
                    state.cancel(None, Some(self.entry.pid));
                }
            }
        }
        let exited_at = *self.leader_exit.get_or_insert_with(Instant::now);
        let idle = self.last_output.max(exited_at).elapsed();
        let cancelled = state
            .cancel
            .as_ref()
            .filter(|cancel| cancel.terminal_pid != Some(self.entry.pid));
        if let Some(cancel) = cancelled {
            if self.entry.target < 0 && cancel.escalation != Escalation::Kill {
                return Ok(None);
            }
            if !drained && cancel.start.elapsed() < KILL_AFTER + DRAIN_AFTER_KILL {
                return Ok(None);
            }
        } else if !drained {
            // Give descendants time to finish the stream after the leader exits.
            // A detached process can inherit the pipe, so idle drain is bounded.
            if display_pending || idle < DRAIN_IDLE {
                self.draining = true;
                return Ok(None);
            }
            if self.entry.target < 0 && self.group_cleanup.is_none() && self.terminal.is_none() {
                signal_target(self.entry.target, libc::SIGKILL);
                self.group_cleanup = Some(Instant::now());
                return Ok(None);
            }
            if self
                .group_cleanup
                .is_some_and(|when| when.elapsed() < DRAIN_AFTER_KILL)
            {
                return Ok(None);
            }
        }
        if let Some(job) = &mut self.terminal {
            // Check the fence only after restoring foreground. Its reply comes
            // after any terminal-generated interrupt queued for the old group.
            if !job.fence()? {
                return Ok(None);
            }
            if job.notices()? {
                state.terminal_interrupt(self.entry.pid, self.draining);
            }
        }
        if self.entry.target < 0 {
            signal_target(self.entry.target, libc::SIGKILL);
        }
        let status = self.child.wait()?;
        self.reaped = true;
        if let Some(cancel) = &mut state.cancel
            && cancel.foreground == Some(self.entry.pid)
        {
            cancel.status = status
                .code()
                .filter(|code| *code != 0)
                .map(|code| code as u8);
        }
        // Remove the PID before releasing the lock to other signalling threads.
        // Running still owns terminal cleanup and the completion barrier.
        state
            .children
            .iter_mut()
            .find(|entry| entry.pid == self.entry.pid)
            .unwrap()
            .target = 0;
        Ok(Some(status))
    }

    fn output_progress(&mut self) {
        self.last_output = Instant::now();
    }

    fn discard_display(&self) -> bool {
        self.supervisor
            .state()
            .cancel
            .as_ref()
            .is_some_and(|cancel| {
                cancel.terminal_pid != Some(self.entry.pid) || self.leader_exit.is_some()
            })
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut state = self.supervisor.state();
        if !self.reaped {
            signal_target(self.entry.target, libc::SIGKILL);
            // Signal the leader separately if a mixed-privilege group accepted
            // SIGKILL only for some of its members.
            if self
                .leader_kill
                .unwrap_or_else(|| signal_target(self.entry.pid, libc::SIGKILL))
            {
                let _ = self.child.wait();
            } else {
                let _ = self.child.try_wait();
            }
        }
        drop(self.terminal.take());
        state.children.retain(|entry| entry.pid != self.entry.pid);
        self.supervisor.idle.notify_all();
    }
}

enum Observed {
    Running,
    Stopped,
    Exited { signal: Option<libc::c_int> },
}

fn observe(pid: libc::pid_t, stops: bool) -> io::Result<Observed> {
    // As in the PTY spike, WNOWAIT reserves the leader PID through cleanup.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let mut options = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if stops {
        options |= libc::WSTOPPED;
    }
    let result = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, options) };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(Observed::Running);
        }
        return Err(error);
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(Observed::Running);
    }
    match info.si_code {
        libc::CLD_STOPPED | libc::CLD_TRAPPED => Ok(Observed::Stopped),
        libc::CLD_KILLED | libc::CLD_DUMPED => Ok(Observed::Exited {
            signal: Some(unsafe { info.si_status() }),
        }),
        _ => Ok(Observed::Exited { signal: None }),
    }
}

fn acknowledge_stop(pid: libc::pid_t) -> io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WSTOPPED | libc::WNOHANG,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } != 0)
}

fn signal_target(target: libc::pid_t, signal: libc::c_int) -> bool {
    if target == 0 || unsafe { libc::kill(target, signal) } == 0 {
        return true;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH)
        || (error.raw_os_error() == Some(libc::EPERM)
            && if target < 0 {
                zombie_only_group(-target)
            } else {
                matches!(observe(target, false), Ok(Observed::Exited { .. }))
            })
    {
        return true;
    }
    eprintln!("dots: could not signal the command; its processes may still be running ({error})");
    false
}

#[cfg(target_os = "macos")]
fn zombie_only_group(group: libc::pid_t) -> bool {
    // Darwin can return EPERM when the group contains only zombies. Inspect
    // membership while our unreaped child still reserves the group ID.
    let mut pids = [0 as libc::pid_t; 32];
    let count = unsafe {
        *libc::__error() = 0;
        let count = libc::proc_listpgrppids(
            group,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as libc::c_int,
        );
        if *libc::__error() != 0 {
            return false;
        }
        count as usize
    };
    if count >= pids.len() {
        return false;
    }
    pids[..count].iter().all(|&pid| {
        let current = unsafe { libc::getpgid(pid) };
        if current < 0 {
            io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        } else {
            current != group
        }
    })
}

#[cfg(not(target_os = "macos"))]
fn zombie_only_group(_group: libc::pid_t) -> bool {
    false
}
