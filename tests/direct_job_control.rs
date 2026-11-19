//! Executable acceptance for DIRECT. Only the paired kernel observation uses a stand-in.
#[path = "direct_job_control/fixture.rs"]
mod fixture;
mod support;
#[path = "direct_job_control/unix.rs"]
mod unix;

use fixture::{Identity, PROMPT, SUDO_INPUT, SUDO_PROMPT};
use nix::libc;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};
use support::{FixtureCommand, World};
use unix::*;

// Keep this suite's PTY sessions sequential under the default Rust runner.
// In particular, openpty cannot set close-on-exec atomically with allocation.
static FIXTURE: Mutex<()> = Mutex::new(());

struct Fixture {
    world: World,
    control: PathBuf,
    started: SystemTime,
    _slot: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let slot = FIXTURE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let started = SystemTime::now();
        let world = World::new();
        let control = world.home().join("direct");
        fixture::install(world.root(), &control);
        fs::create_dir(control.join("tmp")).unwrap();
        Self {
            world,
            control,
            started,
            _slot: slot,
        }
    }

    fn dots(&self) -> FixtureCommand<'_> {
        let mut command = self.world.dots();
        let home = self.world.home();
        command
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "dumb")
            .env("LC_ALL", "C")
            .env("CODEX_HOME", home.join(".codex"))
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .env("MISE_CONFIG_DIR", self.world.root())
            .env("MISE_DATA_DIR", home.join(".local/share/mise"))
            .env("MISE_STATE_DIR", home.join(".local/state/mise"))
            .env("MISE_CACHE_DIR", home.join(".cache/mise"))
            .env("TMPDIR", self.control.join("tmp"))
            .env("DOTS_MISE_BIN", self.control.join("mise"))
            .env("DIRECT_DIR", &self.control)
            .env("DIRECT_EXE", env::current_exe().unwrap());
        command
    }

    fn launch_times(&self) -> Vec<(&'static str, u128)> {
        ["dots-pid", "mise-start", "script-start", "stage"]
            .into_iter()
            .filter_map(|marker| {
                let modified = fs::metadata(self.control.join(marker))
                    .ok()?
                    .modified()
                    .ok()?;
                Some((
                    marker,
                    modified.duration_since(self.started).ok()?.as_millis(),
                ))
            })
            .collect()
    }

    fn live(&self, role: &str) -> bool {
        !fixture::lock(&self.control.join(format!("{role}.live")))
    }

    fn report(&self) -> Option<serde_json::Value> {
        let directory = self.world.home().join(".local/state/dots/reports");
        let files: Vec<_> = fs::read_dir(directory)
            .ok()?
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        let report = files.first()?;
        assert_eq!(files.len(), 1, "expected a single dots test report");
        Some(serde_json::from_reader(File::open(report).unwrap()).unwrap())
    }
}

struct Session<'a> {
    child: Child,
    fixture: &'a Fixture,
}

impl Session<'_> {
    fn pid(&self) -> Pid {
        self.child.id() as Pid
    }

    fn members(&self) -> Vec<Pid> {
        let sid = self.pid();
        let mut members: Vec<_> = processes()
            .into_iter()
            .filter(|pid| *pid > 1 && session(*pid) == sid)
            .collect();
        members.sort_unstable();
        members
    }

    fn wait(&mut self) -> ExitStatus {
        wait_for("fixture session to exit", || self.child.try_wait().unwrap())
    }

    fn identity(&self, role: &str) -> Identity {
        wait_for_with(
            role,
            || Identity::read(&self.fixture.control.join(role)),
            || self.launch_details(),
        )
    }

    fn launch_details(&self) -> String {
        let members: Vec<_> = self
            .members()
            .into_iter()
            .map(|pid| (pid, group(pid), stopped(pid), process_name(pid)))
            .collect();
        let mut load = [0.0; 3];
        let count = unsafe { libc::getloadavg(load.as_mut_ptr(), 3) }.max(0) as usize;
        format!(
            "fixture launch markers (elapsed ms): {:?}, session processes (pid, group, stopped, name): {members:?}, host load: {:?}",
            self.fixture.launch_times(),
            &load[..count],
        )
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        // This session was created solely for the fixture. Also account for
        // explicitly registered fixture children if the binary relocated them.
        let session_id = self.pid();
        for role in ["grandchild", "stage"] {
            if let Some(child) = Identity::read(&self.fixture.control.join(role))
                && child.session != session_id
                && self.fixture.live(role)
                && session(child.pid) == child.session
                && group(child.pid) == child.group
            {
                let _ = signal(child.pid, libc::SIGKILL);
            }
        }
        for pid in self.members().into_iter().rev() {
            if session(pid) == session_id {
                let _ = signal(pid, libc::SIGKILL);
            }
        }
        let end = Instant::now() + Duration::from_secs(2);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

const SHELL_PROMPT: &str = "fixture-shell> ";
const JOB: &str = r#"bash -c 'printf "%s\n" "$$" > "$DIRECT_DIR/dots-pid"; exec "$DIRECT_DOTS" test "$DIRECT_CASE"'"#;
const OBSERVE_JOB: &str = r#"bash -c 'printf "%s\n" "$$" > "$DIRECT_DIR/dots-pid"; DIRECT_ROLE=observe exec "$DIRECT_EXE" --exact new_foreground_ctrl_c_observation_without_witness --nocapture'"#;
const RESULT: &str = r#"printf '\nshell-result:%s\n' "$?""#;

struct Terminal<'a> {
    session: Session<'a>,
    master: File,
    slave: File,
    path: String,
    transcript: Vec<u8>,
    cursor: usize,
    observer: Option<Pid>,
    newline: &'static str,
    case: String,
}

impl<'a> Terminal<'a> {
    fn start(fixture: &'a Fixture, case: &str, tostop: bool) -> Self {
        let mut dots = fixture.dots();
        dots.env("DIRECT_CASE", case);
        Self::shell(fixture, dots, tostop, "\r\n")
    }

    fn shell(
        fixture: &'a Fixture,
        dots: FixtureCommand<'_>,
        tostop: bool,
        newline: &'static str,
    ) -> Self {
        let case = dots
            .get_envs()
            .find(|(key, _)| *key == "DIRECT_CASE")
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (master, slave) = open_pty();
        let mut modes = termios(slave.as_raw_fd());
        modes.c_lflag |= libc::ISIG | libc::ICANON | libc::ECHO;
        modes.c_oflag = if newline == "\r\n" {
            libc::OPOST | libc::ONLCR
        } else {
            0
        };
        if tostop {
            modes.c_lflag |= libc::TOSTOP;
        } else {
            modes.c_lflag &= !libc::TOSTOP;
        }
        modes.c_cc[libc::VINTR] = 3;
        modes.c_cc[libc::VSUSP] = 26;
        set_termios(slave.as_raw_fd(), &modes);
        nonblocking(&master);
        let path = tty_name(slave.as_raw_fd()).unwrap();
        let mut shell = fixture.world.command("/bin/bash");
        shell
            .args(["--noprofile", "--norc", "--noediting", "-i"])
            .envs(
                dots.get_envs()
                    .filter_map(|(key, value)| Some((key, value?))),
            )
            .env("DIRECT_DOTS", dots.get_program())
            .env("PS1", SHELL_PROMPT)
            .env("BASH_SILENCE_DEPRECATION_WARNING", "1")
            .current_dir(dots.get_current_dir().unwrap())
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        // The disposable shell has its own session and controlling terminal.
        unsafe {
            shell.pre_exec(|| {
                check(libc::setsid())?;
                acquire_controlling_tty()?;
                check(libc::tcsetpgrp(0, libc::getpgrp()))
            });
        }
        let child = shell.spawn().unwrap();
        let mut terminal = Self {
            session: Session { child, fixture },
            master,
            slave,
            path,
            transcript: Vec::new(),
            cursor: 0,
            observer: None,
            newline,
            case,
        };
        terminal.expect(SHELL_PROMPT);
        terminal
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
    }

    fn line(&mut self, line: &str) {
        self.send(line.as_bytes());
        self.send(b"\n");
    }

    fn read_pty(&mut self) {
        let mut buffer = [0; 4096];
        match self.master.read(&mut buffer) {
            Ok(count) => self.transcript.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("reading PTY: {error}"),
        }
    }

    fn expect(&mut self, marker: &str) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(offset) = self.transcript[self.cursor..]
                .windows(marker.len())
                .position(|window| window == marker.as_bytes())
            {
                self.cursor += offset + marker.len();
                return;
            }
            assert!(
                Instant::now() < deadline,
                "PTY case {:?} did not show {marker:?}, {}, transcript:\n{}",
                self.case,
                self.session.launch_details(),
                String::from_utf8_lossy(&self.transcript)
            );
            self.read_pty();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn dots_pid(&self) -> Pid {
        wait_for("shell's dots job PID", || {
            fs::read_to_string(self.session.fixture.control.join("dots-pid"))
                .ok()?
                .trim()
                .parse()
                .ok()
        })
    }

    fn start_foreground(&mut self) {
        self.line(&format!("{JOB}; {RESULT}"));
    }

    fn verify_original_tty(&self, child: &Identity, redirected_stdout: bool) {
        let path = self.path.as_str();
        let stdout = if redirected_stdout { "-" } else { path };
        assert_eq!(child.tty, [path, stdout, path], "{child:?}");
        assert_eq!(child.session, self.session.pid(), "{child:?}");
        assert_eq!(child.foreground, child.dev_tty_foreground, "{child:?}");
    }

    fn observe_foreground_group(&mut self, stage: &Identity, grandchild: Option<&Identity>) {
        self.verify_original_tty(stage, false);
        let dots = self.dots_pid();
        assert_ne!(
            stage.group,
            group(dots),
            "stage must have its own group: {stage:?}"
        );
        assert_eq!(stage.group, stage.foreground, "{stage:?}");
        let mut known = vec![self.session.pid(), dots, stage.pid];
        if let Some(child) = grandchild {
            assert_eq!(child.group, stage.group, "{child:?}");
            known.push(child.pid);
        }
        let extra: Vec<_> = self
            .session
            .members()
            .into_iter()
            .filter(|pid| !known.contains(pid))
            .collect();
        assert_eq!(
            extra.len(),
            1,
            "expected one foreground observer, known {known:?}, extra {extra:?}"
        );
        assert_eq!(
            group(extra[0]),
            stage.group,
            "observer must be in the foreground group"
        );
        self.observer = Some(extra[0]);
    }

    fn result(&mut self, code: i32) {
        let newline = self.newline;
        self.expect(&format!("{newline}shell-result:{code}{newline}"));
        self.expect(SHELL_PROMPT);
        // Reading another command proves the interactive shell regained its
        // controlling tty. TOSTOP also guards its writes in foreground cases.
        self.line("printf '\\nshell-can-read\\n'");
        self.expect(&format!("{newline}shell-can-read{newline}"));
        self.assert_observer_gone();
    }

    fn resume_job(&mut self, dots: Pid, stage: Pid) {
        self.line(&format!("fg; {RESULT}"));
        wait_for_with(
            "Bash fg to resume the original dots job and stage",
            || {
                [dots, stage]
                    .into_iter()
                    .all(|pid| session(pid) == self.session.pid() && !stopped(pid))
                    .then_some(())
            },
            || self.session.launch_details(),
        );
    }

    fn complete_resumed_read(&mut self, stage: &Identity, code: i32) {
        self.line("ordinary input");
        self.expect("ordinary input accepted");
        let resumed = self.session.identity("resumed");
        self.verify_original_tty(&resumed, false);
        assert_eq!(resumed.pid, stage.pid, "fg replaced the original reader");
        assert_eq!(resumed.foreground, resumed.group, "{resumed:?}");
        self.result(code);
        assert!(!self.session.fixture.live("stage"));
        assert_eq!(self.session.members(), vec![self.session.pid()]);
    }

    fn assert_observer_gone(&self) {
        if let Some(pid) = self.observer {
            assert_eq!(
                signal(pid, 0).unwrap_err().raw_os_error(),
                Some(libc::ESRCH),
                "observer {pid} survived shell recovery"
            );
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JobState {
    foreground_pgrp: Pid,
    dots_stopped: Option<bool>,
    stage_stopped: Option<bool>,
}

struct FgTrace {
    dots: Pid,
    stage: Pid,
    start: Instant,
    transitions: Vec<(u128, &'static str, JobState)>,
}

impl FgTrace {
    fn sample(&mut self, terminal: &Terminal<'_>, phase: &'static str) -> JobState {
        let owned = |pid| (session(pid) == terminal.session.pid()).then(|| stopped(pid));
        let state = JobState {
            foreground_pgrp: foreground(terminal.master.as_raw_fd()),
            dots_stopped: owned(self.dots),
            stage_stopped: owned(self.stage),
        };
        if self.transitions.len() < 64
            && self
                .transitions
                .last()
                .is_none_or(|&(_, last, previous)| last != phase || previous != state)
        {
            self.transitions
                .push((self.start.elapsed().as_millis(), phase, state));
        }
        state
    }

    fn wait(
        &mut self,
        terminal: &mut Terminal<'_>,
        phase: &'static str,
        ready: impl Fn(JobState) -> bool,
    ) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if ready(self.sample(terminal, phase)) {
                return;
            }
            terminal.read_pty();
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {phase}, {}\n{}",
                terminal.session.launch_details(),
                self.describe(terminal),
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn describe(&self, terminal: &Terminal<'_>) -> String {
        let tail = &terminal.transcript[terminal.transcript.len().saturating_sub(8192)..];
        let mut description = format!(
            "fixture Bash={}, dots={}, original reader={}\n",
            terminal.session.pid(),
            self.dots,
            self.stage,
        );
        use std::fmt::Write as _;
        for (ms, phase, state) in &self.transitions {
            writeln!(
                description,
                "{ms:>4} ms {phase}: foreground_pgrp={} dots_stopped={:?} stage_stopped={:?}",
                state.foreground_pgrp, state.dots_stopped, state.stage_stopped,
            )
            .unwrap();
        }
        write!(
            description,
            "PTY transcript tail: {:?}",
            String::from_utf8_lossy(tail)
        )
        .unwrap();
        description
    }
}

fn drain(reader: &mut File, bytes: &mut Vec<u8>) -> bool {
    let mut buffer = [0; 4096];
    match reader.read(&mut buffer) {
        Ok(0) => true,
        Ok(count) => {
            bytes.extend_from_slice(&buffer[..count]);
            false
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("reading redirected output: {error}"),
    }
}

fn redirected_output(session: &mut Session<'_>) -> (Vec<u8>, Vec<u8>) {
    let mut stdout = File::from(OwnedFd::from(session.child.stdout.take().unwrap()));
    let mut stderr = File::from(OwnedFd::from(session.child.stderr.take().unwrap()));
    nonblocking(&stdout);
    nonblocking(&stderr);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    wait_for("both slow readers to reach EOF", || {
        let out_done = drain(&mut stdout, &mut out);
        let err_done = drain(&mut stderr, &mut err);
        (out_done && err_done).then_some(())
    });
    (out, err)
}

#[test]
fn unchanged_direct_preserves_redirected_bytes_under_backpressure() {
    let fixture = Fixture::new();
    let mut command = fixture.dots();
    command
        .args(["test", "streams", "two words", ""])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| check(libc::setsid()));
    }
    let mut session = Session {
        child: command.spawn().unwrap(),
        fixture: &fixture,
    };
    session
        .child
        .stdin
        .take()
        .unwrap()
        .write_all(b"fixture input\n")
        .unwrap();
    let (out, err) = redirected_output(&mut session);
    assert_eq!(session.wait().code(), Some(23));
    let mut expected_out = b"stdin:[fixture input] argv:[two words][]\n".to_vec();
    expected_out.extend(fixture::OUT.repeat(4096));
    assert!(
        out == expected_out,
        "stdout differed: got {} bytes, expected {}",
        out.len(),
        expected_out.len()
    );
    let expected_err = fixture::ERR.repeat(4096);
    assert!(
        err == expected_err,
        "stderr differed: got {} bytes, expected {}; suffix {:?}",
        err.len(),
        expected_err.len(),
        String::from_utf8_lossy(err.get(expected_err.len()..).unwrap_or_default())
    );
    assert!(
        session.members().is_empty(),
        "process survived redirected execution"
    );
}

#[test]
fn unchanged_native_sudo_prompt_uses_original_tty_without_echo() {
    for redirected_stdout in [false, true] {
        let fixture = Fixture::new();
        let mut terminal = Terminal::start(&fixture, "sudo", true);
        if redirected_stdout {
            terminal.line(&format!("{JOB} >\"$DIRECT_DIR/stdout\"; {RESULT}"));
        } else {
            terminal.start_foreground();
        }
        terminal.expect(SUDO_PROMPT);
        let stage = terminal.session.identity("stage");
        terminal.verify_original_tty(&stage, redirected_stdout);
        assert_eq!(stage.group, stage.foreground);
        assert_eq!(termios(terminal.slave.as_raw_fd()).c_lflag & libc::ECHO, 0);
        terminal.send(format!("{SUDO_INPUT}\n").as_bytes());
        terminal.expect("stub sudo accepted");
        terminal.result(0);
        assert_ne!(termios(terminal.slave.as_raw_fd()).c_lflag & libc::ECHO, 0);
        assert!(
            !terminal
                .transcript
                .windows(SUDO_INPUT.len())
                .any(|data| data == SUDO_INPUT.as_bytes())
        );
        assert_eq!(terminal.session.members(), vec![terminal.session.pid()]);
    }
}

#[test]
fn new_foreground_ctrl_c_observation_without_witness() {
    if let Ok(role) = env::var("DIRECT_ROLE") {
        fixture::run_role(&role);
    }
    // Paired kernel experiment only. Each trial uses a real controlling tty
    // and a stand-in that can see its own SIGINTs and child's wait status.
    for (case, press_ctrl_c, status) in [
        ("ignore-alone", false, 0),
        ("ignore-alone", true, 0),
        ("trap-zero", false, 0),
        ("trap-zero", true, 0),
        ("trap-23", true, 23),
    ] {
        let fixture = Fixture::new();
        let mut terminal = Terminal::start(&fixture, case, true);
        terminal.line(&format!("{OBSERVE_JOB}; {RESULT}"));
        terminal.expect(PROMPT);
        let child = terminal.session.identity("stage");
        terminal.verify_original_tty(&child, false);
        assert_eq!(child.foreground, child.group);
        assert_ne!(child.group, group(terminal.dots_pid()));
        if press_ctrl_c {
            terminal.send(b"\x03");
        }
        if case == "ignore-alone" || !press_ctrl_c {
            terminal.line("ordinary input");
            terminal.expect("ordinary input accepted");
        } else {
            terminal.expect("native SIGINT handler");
        }
        terminal.result(status);
        assert_eq!(
            fs::read_to_string(fixture.control.join("observation")).unwrap(),
            format!("0 {status}\n"),
            "{case}, Ctrl-C {press_ctrl_c}"
        );
    }
}

#[test]
fn new_foreground_transfers_original_tty_and_reclaims_it_on_normal_exit() {
    let fixture = Fixture::new();
    let mut terminal = Terminal::start(&fixture, "line", true);
    terminal.start_foreground();
    terminal.expect(PROMPT);
    terminal.observe_foreground_group(&terminal.session.identity("stage"), None);
    terminal.line("ordinary input");
    terminal.expect("ordinary input accepted");
    terminal.result(0);
}

#[test]
fn new_native_ctrl_c_preserves_nonzero_and_retires_observer_on_cancel() {
    for (case, code) in [("trap-23", 23), ("trap-zero", 130), ("line", 130)] {
        let fixture = Fixture::new();
        let mut terminal = Terminal::start(&fixture, case, true);
        terminal.start_foreground();
        terminal.expect(PROMPT);
        terminal.observe_foreground_group(&terminal.session.identity("stage"), None);
        terminal.send(b"\x03");
        if case != "line" {
            terminal.expect("native SIGINT handler");
        }
        terminal.result(code);
    }
}

#[test]
fn new_first_terminal_ctrl_c_leaves_ignoring_child_interactive_then_cleans_remnant() {
    let fixture = Fixture::new();
    let mut terminal = Terminal::start(&fixture, "ignoring", true);
    terminal.start_foreground();
    terminal.expect(PROMPT);
    let stage = terminal.session.identity("stage");
    let grandchild = terminal.session.identity("grandchild");
    terminal.observe_foreground_group(&stage, Some(&grandchild));
    terminal.send(b"\x03");
    // Allow the supervisor to process the interrupt before releasing the child.
    // A fake ordinary-success exit is also rejected by the required status 130.
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        fixture.live("stage"),
        "one terminal interrupt killed the ignoring foreground leader"
    );
    assert!(
        fixture.report().is_none(),
        "report saved while the foreground child was active"
    );
    terminal.line("ordinary input");
    terminal.expect("ordinary input accepted");
    let report = wait_for("cancelled test report", || fixture.report());
    assert!(
        !fixture.live("grandchild"),
        "report saved before the stubborn remnant was killed"
    );
    terminal.result(130);
    assert_eq!(report["command"], "test");
    assert_eq!(report["status"], 130);
    assert_eq!(report["cancelled"], true);
    assert_eq!(report["stages"][0]["outcome"], "cancelled");
    assert_eq!(report["stages"].as_array().unwrap().len(), 1);
    assert!(report["log_id"].is_null());
    assert!(!report.to_string().contains("ordinary input accepted"));
}

#[test]
fn new_normal_direct_exit_cleans_stubborn_grandchild_and_observer() {
    let fixture = Fixture::new();
    let mut terminal = Terminal::start(&fixture, "leader-exits", true);
    terminal.start_foreground();
    terminal.expect(PROMPT);
    let stage = terminal.session.identity("stage");
    let grandchild = terminal.session.identity("grandchild");
    terminal.observe_foreground_group(&stage, Some(&grandchild));
    assert!(fixture.live("grandchild"));
    terminal.line("ordinary input");
    terminal.expect("ordinary input accepted");
    terminal.result(0);
    assert!(
        !fixture.live("grandchild"),
        "shell recovered while the leaderless group was still running"
    );
}

#[test]
fn new_terminal_ctrl_z_and_shell_fg_stop_and_resume_dots_and_stage_group() {
    let fixture = Fixture::new();
    let mut terminal = Terminal::start(&fixture, "suspend", true);
    // Leave the foreground command alone on this line. Bash must regain the
    // terminal when its job stops, then resume that same job with builtin fg.
    terminal.line(JOB);
    terminal.expect(PROMPT);
    let stage = terminal.session.identity("stage");
    let grandchild = terminal.session.identity("grandchild");
    terminal.observe_foreground_group(&stage, Some(&grandchild));
    let dots = terminal.dots_pid();
    terminal.send(b"\x1a");
    terminal.expect("Stopped");
    terminal.expect(SHELL_PROMPT);
    wait_for(
        "dots and both original child processes to be stopped",
        || {
            [dots, stage.pid, grandchild.pid]
                .into_iter()
                .all(stopped)
                .then_some(())
        },
    );
    terminal.line(&format!("fg; {RESULT}"));
    wait_for(
        "fg to resume dots and both original child processes",
        || {
            [dots, stage.pid, grandchild.pid]
                .into_iter()
                .all(|pid| session(pid) == terminal.session.pid() && !stopped(pid))
                .then_some(())
        },
    );
    assert!(
        fixture.live("grandchild"),
        "fg dropped the original grandchild"
    );
    terminal.line("ordinary input");
    terminal.expect("ordinary input accepted");
    terminal.result(0);
}

#[test]
fn new_background_dots_never_takes_shell_terminal_or_starts_observer() {
    let fixture = Fixture::new();
    let mut terminal = Terminal::start(&fixture, "background", false);
    terminal.line(&format!("{JOB} & job=$!"));
    terminal.expect(SHELL_PROMPT);
    let stage = terminal.session.identity("stage");
    terminal.verify_original_tty(&stage, false);
    assert_eq!(stage.foreground, terminal.session.pid(), "{stage:?}");
    let dots = terminal.dots_pid();
    let mut known = vec![terminal.session.pid(), dots, stage.pid];
    known.sort_unstable();
    assert_eq!(terminal.session.members(), known);
    terminal.line("printf '\\nshell-owns-tty\\n'");
    terminal.expect("\r\nshell-owns-tty\r\n");
    terminal.line(&format!("kill -TERM \"$job\"; wait \"$job\"; {RESULT}"));
    terminal.result(130);
    assert!(!fixture.live("stage"));
    assert_eq!(terminal.session.members(), vec![terminal.session.pid()]);
}

#[test]
fn new_intentional_background_stop_resumes_with_bash_bg() {
    let fixture = Fixture::new();
    let mut terminal = Terminal::start(&fixture, "background-stop", false);
    terminal.line(&format!("{JOB} & job=$!"));
    terminal.expect(SHELL_PROMPT);
    let stage = terminal.session.identity("stage");
    let dots = terminal.dots_pid();
    terminal.verify_original_tty(&stage, false);
    let bash = terminal.session.pid();
    assert_eq!(stage.foreground, bash);
    fs::write(fixture.control.join("stop-stage"), b"").unwrap();
    wait_for_with(
        "intentional SIGSTOP to stop the stage and its Bash job",
        || (stopped(stage.pid) && stopped(dots)).then_some(()),
        || terminal.session.launch_details(),
    );

    terminal.line("jobs -sp");
    terminal.expect(&format!("\r\n{dots}\r\n"));
    terminal.expect(SHELL_PROMPT);
    terminal.line("bg; printf '\\nbg-status:%s\\n' \"$?\"");
    terminal.expect("\r\nbg-status:0\r\n");
    let resumed = terminal.session.identity("resumed");
    assert_eq!(resumed.pid, stage.pid);
    terminal.verify_original_tty(&resumed, false);
    assert_eq!(resumed.foreground, bash);
    assert_eq!(foreground(terminal.master.as_raw_fd()), bash);
    let mut known = vec![bash, dots, stage.pid];
    known.sort_unstable();
    assert_eq!(terminal.session.members(), known, "background observer");

    fs::write(fixture.control.join("finish-stage"), b"").unwrap();
    terminal.line(&format!("wait \"$job\"; {RESULT}"));
    terminal.result(0);
    assert!(!fixture.live("stage"));
}

#[test]
fn new_wrapped_intentional_stop_resumes_with_one_bash_fg() {
    let fixture = Fixture::new();
    let mut terminal = Terminal::start(&fixture, "background-stop", false);
    terminal.line(&format!(
        r#"{{ {JOB}; code=$?; printf '\nwrapper-after:%s\n' "$code"; exit "$code"; }} & job=$!"#,
    ));
    terminal.expect(SHELL_PROMPT);
    terminal.line(r#"printf '%s\n' "$job" > "$DIRECT_DIR/wrapper-pid"; printf '\nwrapper-launched:%s\n' "$job""#);
    terminal.expect("\r\nwrapper-launched:");
    terminal.expect(SHELL_PROMPT);
    let wrapper: Pid = fs::read_to_string(fixture.control.join("wrapper-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let dots = terminal.dots_pid();
    let stage = terminal.session.identity("stage");
    let bash = terminal.session.pid();
    assert_ne!(wrapper, dots);
    assert_eq!(group(wrapper), wrapper);
    assert_eq!(group(dots), wrapper);
    assert_ne!(stage.group, wrapper);
    assert_eq!(stage.foreground, bash);
    terminal.verify_original_tty(&stage, false);
    terminal.line("jobs -pr; PS4='+ fixture-bash: '; set -x");
    terminal.expect(&format!("\r\n{wrapper}\r\n"));
    terminal.expect(SHELL_PROMPT);

    let mut trace = FgTrace {
        dots,
        stage: stage.pid,
        start: Instant::now(),
        transitions: Vec::new(),
    };
    fs::write(fixture.control.join("stop-stage"), b"").unwrap();
    trace.wait(&mut terminal, "wrapped stage kernel stop", |state| {
        state.stage_stopped == Some(true)
    });
    assert!(!stopped(wrapper));
    if env::var_os("DOTS_TEST_FG_TRACE").is_some() {
        eprintln!("before fg: {}", terminal.session.launch_details());
    }
    terminal.line(&format!("fg; {RESULT}"));
    trace.wait(
        &mut terminal,
        "single fg to resume the wrapped stage",
        |state| {
            state.dots_stopped == Some(false)
                && state.stage_stopped == Some(false)
                && state.foreground_pgrp == stage.group
        },
    );
    if env::var_os("DOTS_TEST_FG_TRACE").is_some() {
        eprintln!("after fg: {}", terminal.session.launch_details());
    }
    let resumed = terminal.session.identity("resumed");
    assert_eq!(resumed.pid, stage.pid);
    assert_eq!(resumed.foreground, stage.group);
    terminal.verify_original_tty(&resumed, false);
    assert!(!stopped(wrapper));

    fs::write(fixture.control.join("finish-stage"), b"").unwrap();
    terminal.expect("\r\nwrapper-after:0\r\n");
    terminal.result(0);
    trace.sample(&terminal, "shell read after wrapper marker");
    assert_eq!(terminal.session.members(), vec![bash]);
    if env::var_os("DOTS_TEST_FG_TRACE").is_some() {
        eprintln!(
            "wrapper={wrapper} pgrp={wrapper}\n{}",
            trace.describe(&terminal)
        );
    }
}

#[test]
fn new_background_tty_read_stops_and_bash_fg_resumes_native_input() {
    let fixture = Fixture::new();
    let mut terminal = Terminal::start(&fixture, "background-reader", false);
    terminal.line(&format!("{JOB} &"));
    terminal.expect(SHELL_PROMPT);
    let stage = terminal.session.identity("stage");
    let dots = terminal.dots_pid();
    terminal.verify_original_tty(&stage, false);
    assert_eq!(stage.foreground, terminal.session.pid(), "{stage:?}");
    assert_ne!(stage.group, stage.foreground, "{stage:?}");
    let mut known = vec![terminal.session.pid(), dots, stage.pid];
    known.sort_unstable();
    assert_eq!(terminal.session.members(), known, "background observer");
    // Bash xtrace distinguishes execution of fg from terminal input echo.
    // Collect the initial job table before letting the reader reach SIGTTIN.
    terminal.line("jobs -l; PS4='+ fixture-bash: '; set -x; printf '\\nshell-owns-tty\\n'");
    terminal.expect("\r\nshell-owns-tty\r\n");
    terminal.expect(SHELL_PROMPT);

    let mut trace = FgTrace {
        dots,
        stage: stage.pid,
        start: Instant::now(),
        transitions: Vec::new(),
    };
    trace.sample(&terminal, "before releasing background read");
    fs::write(fixture.control.join("read-tty"), b"").unwrap();
    terminal.expect(PROMPT);
    trace.wait(&mut terminal, "reader kernel stop", |state| {
        state.stage_stopped == Some(true)
    });
    terminal.line(r#"fg; fg_status=$?; jobs -l; printf '\nshell-result:%s\n' "$fg_status""#);
    trace.wait(
        &mut terminal,
        "Bash fg to resume dots and original reader",
        |state| state.dots_stopped == Some(false) && state.stage_stopped == Some(false),
    );
    terminal.complete_resumed_read(&stage, 0);
    trace.sample(&terminal, "shell read after exit");
    if env::var_os("DOTS_TEST_FG_TRACE").is_some() {
        eprintln!("{}", trace.describe(&terminal));
    }
}

#[test]
fn new_ignored_ctrl_c_survives_ctrl_z_and_bash_fg_with_cancelled_status() {
    for (case, code) in [("cancel-suspend-zero", 130), ("cancel-suspend-23", 23)] {
        let fixture = Fixture::new();
        let mut terminal = Terminal::start(&fixture, case, true);
        terminal.line(JOB);
        terminal.expect(PROMPT);
        let stage = terminal.session.identity("stage");
        let dots = terminal.dots_pid();
        terminal.observe_foreground_group(&stage, None);

        terminal.send(b"\x03");
        terminal.expect("native SIGINT ignored");
        terminal.line("still reading");
        terminal.expect("native input survived SIGINT");
        assert!(fixture.live("stage"));
        terminal.send(b"\x1a");
        wait_for_with(
            "foreground stage to stop on terminal Ctrl-Z",
            || stopped(stage.pid).then_some(()),
            || terminal.session.launch_details(),
        );
        wait_for_with(
            "Bash's dots job to stop after its interrupted stage stopped",
            || stopped(dots).then_some(()),
            || terminal.session.launch_details(),
        );
        terminal.expect("Stopped");
        terminal.expect(SHELL_PROMPT);

        terminal.resume_job(dots, stage.pid);
        terminal.complete_resumed_read(&stage, code);
        let report = fixture.report().expect("saved cancellation report");
        assert_eq!(report["status"], code);
        assert_eq!(report["cancelled"], true);
        assert_eq!(report["stages"][0]["outcome"], "cancelled");
    }
}

#[test]
fn new_detached_signals_clean_stubborn_process_group_without_observer() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let fixture = Fixture::new();
        let mut command = fixture.dots();
        command
            .args(["test", "detached"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| check(libc::setsid()));
        }
        let mut session = Session {
            child: command.spawn().unwrap(),
            fixture: &fixture,
        };
        let stage = session.identity("stage");
        let grandchild = session.identity("grandchild");
        assert_eq!(stage.tty, vec!["-"; 3]);
        assert_eq!((stage.foreground, stage.dev_tty_foreground), (-1, -1));
        assert_eq!(stage.group, grandchild.group);
        assert_ne!(stage.group, group(session.pid()));
        let mut known = vec![session.pid(), stage.pid, grandchild.pid];
        known.sort_unstable();
        assert_eq!(session.members(), known);
        signal(session.pid(), sig).unwrap();
        assert_eq!(session.wait().code(), Some(130), "external signal {sig}");
        for role in ["stage", "grandchild"] {
            assert!(!fixture.live(role), "{role} survived external signal {sig}");
        }
        assert!(
            session
                .members()
                .into_iter()
                .all(|pid| pid == stage.pid || pid == grandchild.pid)
        );
    }
}
