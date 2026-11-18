//! Native stage and probe lifecycle, plus the plain and verbose up presentation.
mod groups;
mod logs;
mod pipe;
mod signals;
mod terminal;
#[cfg(test)]
mod tests;

use nix::libc;
use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::Ctx;
use crate::report::{Command as ReportCommand, Issue, StageId};
use crate::run::{Policy, exit_code};
use logs::Logs;
use pipe::{Console, Pipes};
use signals::Signals;
use terminal::{TerminalJob, foreground_terminal};

pub(crate) use logs::remove_run_logs;
pub use terminal::terminal_observer;

const TICK: Duration = Duration::from_millis(10);
const TERM_AFTER: Duration = Duration::from_millis(150);
const KILL_AFTER: Duration = Duration::from_millis(650);
const OWNED_DRAIN_IDLE: Duration = Duration::from_secs(30);
const DRAIN_AFTER_KILL: Duration = Duration::from_millis(250);
const GROUP_CHECK_TICK: Duration = Duration::from_millis(50);
const PROBE_STOP_CHECK_TICK: Duration = Duration::from_millis(100);
const GROUP_RETIRE_GRACE: Duration = Duration::from_secs(1);
const PROBE_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;

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
    Probe,
    Detached,
    DetachedProbe(Instant),
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

pub(crate) struct Supervisor<'a> {
    ctx: &'a Ctx,
    mode: Mode,
    signals: io::Result<Signals>,
    state: Mutex<State>,
    idle: Condvar,
    log_base: io::Result<PathBuf>,
    logs: Mutex<Logs>,
    #[cfg(test)]
    membership_checks: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
struct State {
    children: Vec<Entry>,
    launching: usize,
    last_signal: libc::c_int,
    cancel: Option<Cancellation>,
    closed: bool,
    warnings: Vec<String>,
}

#[derive(Clone, Copy)]
struct Entry {
    pid: libc::pid_t,
    target: libc::pid_t,
    foreground: bool,
    native: bool,
    verify_wait: bool,
}

struct Cancellation {
    shutdown: Shutdown,
    signal: libc::c_int,
    // TTY line discipline already delivered the interrupt to this group. While
    // its leader still runs, let it decide how to handle the keystroke.
    terminal_pid: Option<libc::pid_t>,
    foreground: Option<libc::pid_t>,
    status: Option<u8>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Escalation {
    #[default]
    Interrupt,
    Terminate,
    Kill,
}

#[derive(Clone, Copy)]
struct Shutdown {
    start: Instant,
    escalation: Escalation,
}

impl Shutdown {
    fn begin() -> Self {
        Self {
            start: Instant::now(),
            escalation: Escalation::Interrupt,
        }
    }

    fn advance(&mut self, mut signal: impl FnMut(libc::c_int)) {
        for (delay, step, signo) in [
            (TERM_AFTER, Escalation::Terminate, libc::SIGTERM),
            (KILL_AFTER, Escalation::Kill, libc::SIGKILL),
        ] {
            if self.start.elapsed() >= delay
                && self.escalation != Escalation::Kill
                && self.escalation != step
            {
                self.escalation = step;
                signal(signo);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct ProbeBudget {
    deadline: Instant,
    received: bool,
    shutdown: Option<Shutdown>,
    stop: Option<ProbeStop>,
    cancelled: bool,
}

#[derive(Clone, Copy)]
enum ProbeStop {
    Deadline,
    OutputLimit,
}

impl ProbeBudget {
    fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            received: false,
            shutdown: None,
            stop: None,
            cancelled: false,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Completion {
    pub(crate) status: u8,
    pub(crate) cancelled: bool,
}

struct PendingLaunch<'a>(&'a Supervisor<'a>);

impl Drop for PendingLaunch<'_> {
    fn drop(&mut self) {
        self.0.state().launching -= 1;
        self.0.idle.notify_all();
    }
}

impl<'a> Supervisor<'a> {
    pub(crate) fn new(ctx: &'a Ctx, mode: Mode) -> Self {
        let signals = Signals::install();
        let last_signal = signals.as_ref().map_or(0, |signals| signals.start);
        Self {
            ctx,
            mode,
            signals,
            state: Mutex::new(State {
                last_signal,
                ..State::default()
            }),
            idle: Condvar::new(),
            log_base: ctx.env.state_dir(),
            logs: Mutex::new(Logs::default()),
            #[cfg(test)]
            membership_checks: std::sync::atomic::AtomicUsize::new(0),
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
        let mut running = self.spawn(command, placement, !capture)?;
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
        let mut drained = false;
        let result = Pipes::take(&mut running.child).and_then(|mut pipes| {
            let result = pipes.pump(&mut running, verbose, |_, bytes| {
                if up {
                    diagnostic.record(bytes);
                }
                Ok(())
            });
            drained = pipes.drained();
            result
        });
        if result.is_err() || !drained {
            diagnostic.incomplete();
        }
        if !up {
            return result;
        }
        let mut logs = self.logs.lock().unwrap();
        if !result.as_ref().is_ok_and(|status| status.success()) {
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
            Placement::Probe
        };
        let mut running = self.spawn(command, placement, false)?;
        Pipes::take(&mut running.child)?.collect(&mut running)
    }

    pub(crate) fn probe_detached(
        &self,
        mut command: Command,
        limit: Duration,
    ) -> io::Result<Output> {
        let deadline = Instant::now().checked_add(limit).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "probe deadline is out of range",
            )
        })?;
        command.stdin(Stdio::null());
        self.spawn_streams(&mut command, true);
        let mut running = self.spawn(command, Placement::DetachedProbe(deadline), false)?;
        running.budget = Some(ProbeBudget::new(deadline));
        let result = Pipes::take(&mut running.child)
            .and_then(|pipes| pipes.collect_limited(&mut running, PROBE_OUTPUT_LIMIT));
        let expired = matches!(
            running.budget.and_then(|budget| budget.stop),
            Some(ProbeStop::Deadline)
        );
        drop(running);
        if self.cancelled() {
            Err(interrupted())
        } else if expired {
            Err(probe_timed_out())
        } else {
            result
        }
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

    fn spawn(
        &self,
        mut command: Command,
        placement: Placement,
        native: bool,
    ) -> io::Result<Running<'_>> {
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
        let deadline = match placement {
            Placement::DetachedProbe(deadline) => Some(deadline),
            _ => None,
        };
        let verify_wait = deadline.is_some();
        if verify_wait {
            require_waitable_children()?;
        }
        let fragment = matches!(self.mode, Mode::Fragment) && !verify_wait;
        let job_control = matches!(placement, Placement::Foreground) && !fragment;
        let terminal = if job_control {
            TerminalJob::start(self.ctx, None)?
        } else {
            None
        };
        if let Some(terminal) = &terminal {
            command.process_group(terminal.group());
        } else if !fragment {
            if matches!(placement, Placement::Detached | Placement::DetachedProbe(_)) {
                unsafe {
                    // setsid makes a new group as well as a new session.
                    command.pre_exec(move || {
                        if libc::setsid() == -1 {
                            return Err(io::Error::last_os_error());
                        }
                        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
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
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(probe_timed_out());
        }
        // Waiting for the kernel's fork/exec handshake must not hold admission.
        // Completion still accounts for this admitted launch until ownership is resolved.
        let launch = if verify_wait {
            state.launching += 1;
            Some(PendingLaunch(self))
        } else {
            None
        };
        let child = if launch.is_some() {
            drop(state);
            let child = command.spawn();
            drop(command);
            state = self.state();
            match child {
                Ok(child) => child,
                Err(error) => {
                    let cancelled = state.cancel.is_some();
                    drop(state);
                    return Err(if cancelled { interrupted() } else { error });
                }
            }
        } else {
            let child = command.spawn()?;
            drop(command);
            child
        };
        let pid = child.id() as libc::pid_t;
        let target = if fragment {
            // A bootstrap callback inherits the outer mise stage's group.
            pid
        } else {
            -terminal.as_ref().map_or(pid, TerminalJob::group)
        };
        let mut entry = Entry {
            pid,
            target,
            foreground: placement.foreground(),
            native,
            verify_wait,
        };
        if launch.is_some()
            && let Some(cancel) = &state.cancel
        {
            let signal = match cancel.shutdown.escalation {
                Escalation::Interrupt => cancel.signal,
                Escalation::Terminate => libc::SIGTERM,
                Escalation::Kill => libc::SIGKILL,
            };
            entry.signal(signal, &mut state.warnings);
        }
        state.children.push(entry);
        state.refresh_signals();
        drop(state);
        drop(launch);
        let probe = matches!(placement, Placement::Probe) && !fragment;
        let mut running = Running {
            supervisor: self,
            child,
            terminal,
            entry,
            job_control,
            budget: None,
            probe,
            probe_stop_check: (probe && terminal::controlling_terminal().is_some())
                .then(Instant::now),
            leader_exit: None,
            last_output: Instant::now(),
            group_cleanup: None,
            membership: None,
            forced_drain: false,
            retirement_done: false,
            retirement_unconfirmed: false,
            draining: false,
            leader_kill: None,
            reaped: false,
        };
        // Transfer from the parent after exec, so terminal stop signals
        // cannot suspend the child before Command::spawn finishes.
        if !self.cancelled()
            && let Some(terminal) = &mut running.terminal
        {
            terminal.enter()?;
        }
        Ok(running)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.refresh_signals();
        state
    }

    pub(crate) fn complete(
        &self,
        command: Option<ReportCommand>,
        status: u8,
        flush: impl FnOnce(),
    ) -> Completion {
        let mut state = self.state();
        state.closed = true;
        while state.launching != 0 || !state.children.is_empty() {
            state = self
                .idle
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
        drop(state);
        flush();
        let state = self.state();
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

    pub(crate) fn stdout(&self) -> Console<'_> {
        Console::new(self, libc::STDOUT_FILENO)
    }

    pub(crate) fn stderr(&self) -> Console<'_> {
        Console::new(self, libc::STDERR_FILENO)
    }

    pub(crate) fn flush_warnings(&self) {
        let mut warnings = std::mem::take(&mut self.state().warnings);
        warnings.extend(self.logs.lock().unwrap().take_warnings());
        let mut stderr = self.stderr();
        for warning in warnings {
            let _ = writeln!(stderr, "{warning}");
        }
    }

    pub(crate) fn replay_alongside(&self, id: StageId, output: &mut impl Write) -> io::Result<()> {
        if matches!(self.mode, Mode::Up { verbose: true, .. }) {
            let replay = self.logs.lock().unwrap().replay(id);
            if let Some(path) = replay {
                let mut log = File::open(path)?;
                let mut bytes = [0; 8192];
                let mut last = None;
                while !self.cancelled() {
                    let count = log.read(&mut bytes)?;
                    if count == 0 {
                        break;
                    }
                    output.write_all(&bytes[..count])?;
                    last = Some(bytes[count - 1]);
                }
                if last.is_some_and(|byte| byte != b'\n') {
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
        cancel.shutdown.advance(|signal| {
            for entry in &mut self.children {
                if Some(entry.pid) != cancel.terminal_pid {
                    entry.signal(signal, &mut self.warnings);
                }
            }
        });
    }

    fn cancel(&mut self, signal: Option<libc::c_int>, terminal_pid: Option<libc::pid_t>) {
        if let Some(cancel) = &mut self.cancel {
            if let Some(signal) = signal
                && let Some(pid) = cancel.terminal_pid.take()
                && let Some(entry) = self.children.iter_mut().find(|entry| entry.pid == pid)
            {
                // An explicit signal to dots supersedes a terminal-only cancel.
                entry.signal(signal, &mut self.warnings);
                cancel.shutdown = Shutdown::begin();
                cancel.signal = signal;
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
        let signal = signal.unwrap_or(libc::SIGINT);
        self.cancel = Some(Cancellation {
            shutdown: Shutdown::begin(),
            signal,
            terminal_pid,
            foreground,
            status: None,
        });
        for entry in &mut self.children {
            if Some(entry.pid) != terminal_pid {
                entry.signal(signal, &mut self.warnings);
            }
        }
    }

    fn terminal_interrupt(&mut self, entry: Entry, during_drain: bool) {
        if self.cancel.is_some() {
            return;
        }
        self.cancel(None, Some(entry.pid));
        if let Some(cancel) = &mut self.cancel {
            if during_drain {
                cancel.foreground = None;
            }
            if !entry.native || during_drain {
                cancel.terminal_pid = None;
            }
        }
    }
}

impl Entry {
    fn signal(&mut self, signal: libc::c_int, warnings: &mut Vec<String>) -> bool {
        if self.verify_wait
            && self.target != 0
            && let Err(error) = observe(self.pid, false)
        {
            self.abandon(&error, warnings);
            return false;
        }
        signal_target(self.target, signal, warnings)
    }

    fn abandon(&mut self, error: &io::Error, warnings: &mut Vec<String>) {
        if self.target != 0 {
            self.target = 0;
            warnings.push(format!(
                "dots: lost ownership of the probe's processes ({error})"
            ));
        }
    }
}

struct Running<'a> {
    supervisor: &'a Supervisor<'a>,
    child: Child,
    terminal: Option<TerminalJob>,
    entry: Entry,
    job_control: bool,
    budget: Option<ProbeBudget>,
    probe: bool,
    probe_stop_check: Option<Instant>,
    leader_exit: Option<Instant>,
    last_output: Instant,
    group_cleanup: Option<Instant>,
    membership: Option<(Instant, Result<bool, String>)>,
    forced_drain: bool,
    retirement_done: bool,
    retirement_unconfirmed: bool,
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
                .terminal_interrupt(self.entry, self.draining);
        }

        let supervisor = self.supervisor;
        let mut state = supervisor.state();
        let observation = self.observe(&mut state, self.job_control || self.probe)?;
        let exited = matches!(observation, Observed::Exited);
        if state.cancel.is_none()
            && (matches!(observation, Observed::Running) || exited && !drained)
            && self.probe_group_stopped()?
        {
            // A launcher can block the job-control signal while its reader stops.
            self.reserve_probe_foreground(&mut state)?;
            drop(state);
            self.handle_stop(libc::SIGTTIN)?;
            return Ok(None);
        }
        if let Some(budget) = &mut self.budget {
            budget.cancelled = state.cancel.is_some();
            if !budget.received && budget.shutdown.is_none() && !budget.cancelled {
                if Instant::now() >= budget.deadline {
                    budget.stop.get_or_insert(ProbeStop::Deadline);
                }
                if budget.stop.is_some() {
                    signal_target(self.entry.target, libc::SIGINT, &mut state.warnings);
                    budget.shutdown = Some(Shutdown::begin());
                } else if exited && drained {
                    // Cleanup of a completed operation is outside the result budget.
                    budget.received = true;
                }
            }
            if let Some(shutdown) = &mut budget.shutdown {
                shutdown.advance(|signal| {
                    signal_target(self.entry.target, signal, &mut state.warnings);
                });
            }
        }
        if !exited
            && self.leader_kill.is_none()
            && self
                .shutdown(&state)
                .is_some_and(|s| s.escalation == Escalation::Kill)
        {
            // A group signal can succeed for its observer or a descendant
            // even when the native leader refuses it.
            let signalled = signal_target(self.entry.pid, libc::SIGKILL, &mut state.warnings);
            self.leader_kill = Some(signalled);
            if !signalled {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "could not stop the interrupted child",
                ));
            }
        }
        if !exited
            && self.budget.is_some()
            && self
                .shutdown(&state)
                .is_some_and(|s| s.start.elapsed() >= KILL_AFTER + GROUP_RETIRE_GRACE)
        {
            return Err(io::Error::other(
                "could not confirm that the probe leader stopped",
            ));
        }
        match observation {
            Observed::Running => return Ok(None),
            Observed::Stopped { signal }
                if state
                    .cancel
                    .as_ref()
                    .is_none_or(|cancel| cancel.terminal_pid == Some(self.entry.pid)) =>
            {
                if self.probe && !self.entry.foreground {
                    if !matches!(signal, libc::SIGTTIN | libc::SIGTTOU) {
                        return Err(io::Error::other("probe stopped"));
                    }
                    self.reserve_probe_foreground(&mut state)?;
                }
                drop(state);
                if acknowledge_stop(self.entry.pid)? {
                    self.handle_stop(signal)?;
                }
                return Ok(None);
            }
            Observed::Stopped { .. } => {
                let _ = acknowledge_stop(self.entry.pid);
                return Ok(None);
            }
            Observed::Exited => {}
        }
        let exited_at = *self.leader_exit.get_or_insert_with(Instant::now);
        let idle = self.last_output.max(exited_at).elapsed();
        if let Some(shutdown) = self.shutdown(&state) {
            if self.entry.target < 0 && shutdown.escalation != Escalation::Kill {
                return Ok(None);
            }
            if !drained && shutdown.start.elapsed() < KILL_AFTER + DRAIN_AFTER_KILL {
                return Ok(None);
            }
        } else if !drained {
            if self.budget.is_some() {
                return Ok(None);
            }
            let delay = if matches!(self.group_members(), Ok(true)) {
                OWNED_DRAIN_IDLE
            } else {
                DRAIN_AFTER_KILL
            };
            if display_pending || idle < delay {
                self.draining = true;
                return Ok(None);
            }
            self.forced_drain = true;
        }
        if let Some(job) = &mut self.terminal {
            // Check the fence only after restoring foreground. Its reply comes
            // after any terminal-generated interrupt queued for the old group.
            if !job.fence()? {
                return Ok(None);
            }
            if job.notices()? {
                state.terminal_interrupt(self.entry, self.draining);
            }
        }
        if !self.retired(&mut state) {
            return Ok(None);
        }
        if display_pending && state.cancel.is_none() {
            return Ok(None);
        }
        let status = match self.child.wait() {
            Ok(status) => status,
            Err(error) => {
                self.abandon(&mut state, &error);
                return Err(error);
            }
        };
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
        if self.entry.verify_wait && self.retirement_unconfirmed {
            Err(io::Error::other("could not confirm probe group retirement"))
        } else if self.forced_drain && state.cancel.is_none() {
            Err(io::Error::other(
                "the stage output stayed open after its leader exited",
            ))
        } else {
            Ok(Some(status))
        }
    }

    fn output_progress(&mut self) {
        self.last_output = Instant::now();
    }

    fn discard_capture(&self) -> bool {
        self.budget
            .is_some_and(|budget| budget.cancelled || budget.stop.is_some())
    }

    fn stop_capture(&mut self) -> bool {
        let Some(budget) = &mut self.budget else {
            return false;
        };
        budget.stop = Some(ProbeStop::OutputLimit);
        true
    }

    fn shutdown(&self, state: &State) -> Option<Shutdown> {
        self.budget.and_then(|budget| budget.shutdown).or_else(|| {
            state
                .cancel
                .as_ref()
                .filter(|cancel| cancel.terminal_pid != Some(self.entry.pid))
                .map(|cancel| cancel.shutdown)
        })
    }

    fn observe(&mut self, state: &mut State, stops: bool) -> io::Result<Observed> {
        if self.entry.verify_wait
            && state
                .children
                .iter()
                .any(|entry| entry.pid == self.entry.pid && entry.target == 0)
        {
            self.entry.target = 0;
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
        observe(self.entry.pid, stops).inspect_err(|error| self.abandon(state, error))
    }

    fn abandon(&mut self, state: &mut State, error: &io::Error) {
        if self.entry.verify_wait {
            if let Some(entry) = state
                .children
                .iter_mut()
                .find(|entry| entry.pid == self.entry.pid)
            {
                entry.abandon(error, &mut state.warnings);
            }
            self.entry.target = 0;
        }
    }

    fn probe_group_stopped(&mut self) -> io::Result<bool> {
        let Some(last) = &mut self.probe_stop_check else {
            return Ok(false);
        };
        if self.entry.foreground || last.elapsed() < PROBE_STOP_CHECK_TICK {
            return Ok(false);
        }
        *last = Instant::now();
        groups::has_stopped_child(-self.entry.target, self.entry.pid)
    }

    fn reserve_probe_foreground(&mut self, state: &mut State) -> io::Result<()> {
        if state.children.iter().any(|child| child.foreground) {
            return Err(io::Error::other(
                "probe needs the terminal while another foreground command is active",
            ));
        }
        self.entry.foreground = true;
        state
            .children
            .iter_mut()
            .find(|child| child.pid == self.entry.pid)
            .unwrap()
            .foreground = true;
        Ok(())
    }

    fn group_members(&mut self) -> Result<bool, String> {
        if self.entry.target >= 0 {
            return Ok(false);
        }
        if let Some((at, result)) = &self.membership
            && at.elapsed() < GROUP_CHECK_TICK
        {
            return result.clone();
        }
        #[cfg(test)]
        self.supervisor
            .membership_checks
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = if self.entry.verify_wait && self.leader_exit.is_some() {
            groups::has_live_after_leader_exit(self.entry.pid)
        } else {
            groups::has_live(
                -self.entry.target,
                self.terminal.as_ref().map(TerminalJob::observer_pid),
            )
        }
        .map_err(|error| error.to_string());
        self.membership = Some((Instant::now(), result.clone()));
        result
    }

    /// Waitable leaders anchor the group until its final live-member check.
    fn retired(&mut self, state: &mut State) -> bool {
        if self.retirement_done {
            return true;
        }
        if self.entry.verify_wait && self.group_cleanup.is_none() {
            // Signal with the leader still waitable, then take a fresh snapshot.
            self.begin_group_retirement(state);
            return false;
        }
        let fresh = self
            .membership
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= GROUP_CHECK_TICK);
        let result = self.group_members();
        if matches!(result, Ok(false)) {
            self.retirement_done = true;
            return true;
        }
        let Some(start) = self.group_cleanup else {
            self.begin_group_retirement(state);
            return false;
        };
        if start.elapsed() < GROUP_RETIRE_GRACE {
            if self.entry.verify_wait && fresh {
                signal_target(self.entry.target, libc::SIGKILL, &mut state.warnings);
            }
            return false;
        }
        let reason = result
            .err()
            .unwrap_or_else(|| "the group still has live members".into());
        state.warnings.push(format!(
            "dots: could not confirm that the command's processes stopped ({reason})"
        ));
        self.retirement_done = true;
        self.retirement_unconfirmed = true;
        true
    }

    fn begin_group_retirement(&mut self, state: &mut State) {
        signal_target(self.entry.target, libc::SIGKILL, &mut state.warnings);
        self.group_cleanup = Some(Instant::now());
        self.membership = None;
    }

    fn handle_stop(&mut self, signal: libc::c_int) -> io::Result<()> {
        let group = -self.entry.target;
        if let Some(job) = &mut self.terminal {
            // A fast native child can read or write its terminal before the
            // parent's post-spawn handoff. Resume that background stop.
            if matches!(signal, libc::SIGTTIN | libc::SIGTTOU) && job.owns_foreground() {
                return terminal::continue_group(group);
            }
            return job.suspend();
        }
        if terminal::controlling_terminal().is_none() {
            return if self.probe {
                Err(io::Error::other(
                    "probe stopped without a controlling terminal",
                ))
            } else {
                Ok(())
            };
        }
        // The shell may foreground dots before we observe its reader stop.
        // If it has not, stop dots too so an ordinary fg can promote it.
        if foreground_terminal().is_none() {
            terminal::suspend_caller()?;
        }
        if let Some(mut job) = TerminalJob::start(self.supervisor.ctx, Some(group))? {
            job.enter()?;
            self.terminal = Some(job);
        }
        terminal::continue_group(group)
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
        if !self.reaped && (!self.entry.verify_wait || self.observe(&mut state, false).is_ok()) {
            self.begin_group_retirement(&mut state);
            // Signal the leader separately if a mixed-privilege group accepted
            // SIGKILL only for some of its members.
            let signalled = self.leader_kill.unwrap_or_else(|| {
                signal_target(self.entry.pid, libc::SIGKILL, &mut state.warnings)
            });
            let until = Instant::now() + GROUP_RETIRE_GRACE;
            loop {
                let observation = self.observe(&mut state, false);
                if self.entry.verify_wait && observation.is_err() {
                    break;
                }
                if self.entry.verify_wait && matches!(observation, Ok(Observed::Exited)) {
                    self.leader_exit.get_or_insert_with(Instant::now);
                }
                let retired = self.retired(&mut state);
                if retired && matches!(observation, Ok(Observed::Exited)) {
                    let _ = self.child.wait();
                    break;
                }
                if retired && (!signalled || Instant::now() >= until) {
                    if signalled {
                        state
                            .warnings
                            .push("dots: could not confirm that the command leader stopped".into());
                    }
                    break;
                }
                drop(state);
                std::thread::sleep(TICK);
                state = self.supervisor.state();
            }
        }
        drop(self.terminal.take());
        state.children.retain(|entry| entry.pid != self.entry.pid);
        self.supervisor.idle.notify_all();
    }
}

enum Observed {
    Running,
    Stopped { signal: libc::c_int },
    Exited,
}

fn require_waitable_children() -> io::Result<()> {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::uninit();
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let action = unsafe { action.assume_init() };
    if action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::other(
            "the probe requires waitable child processes",
        ));
    }
    Ok(())
}

fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "dots run was cancelled")
}

fn probe_timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "the probe exceeded its result deadline",
    )
}

fn observe(pid: libc::pid_t, stops: bool) -> io::Result<Observed> {
    // As in the PTY spike, WNOWAIT reserves the leader PID through cleanup.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let mut options = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if stops {
        options |= libc::WSTOPPED;
    }
    loop {
        if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, options) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(Observed::Running);
    }
    match info.si_code {
        libc::CLD_STOPPED | libc::CLD_TRAPPED => Ok(Observed::Stopped {
            signal: unsafe { info.si_status() },
        }),
        _ => Ok(Observed::Exited),
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

fn signal_target(target: libc::pid_t, signal: libc::c_int, warnings: &mut Vec<String>) -> bool {
    if target == 0 || unsafe { libc::kill(target, signal) } == 0 {
        return true;
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH)
        || (error.raw_os_error() == Some(libc::EPERM)
            && if target < 0 {
                zombie_only_group(-target)
            } else {
                matches!(observe(target, false), Ok(Observed::Exited))
            })
    {
        return true;
    }
    let warning =
        format!("dots: could not signal the command; its processes may still be running ({error})");
    if !warnings.contains(&warning) {
        warnings.push(warning);
    }
    false
}

#[cfg(target_os = "macos")]
fn zombie_only_group(group: libc::pid_t) -> bool {
    matches!(groups::has_live(group, None), Ok(false))
}

#[cfg(not(target_os = "macos"))]
fn zombie_only_group(_group: libc::pid_t) -> bool {
    false
}
