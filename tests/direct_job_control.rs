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
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};
use support::World;
use unix::*;

// Keep this suite's PTY sessions sequential under the default Rust runner.
// In particular, openpty cannot set close-on-exec atomically with allocation.
static FIXTURE: Mutex<()> = Mutex::new(());

struct Fixture {
    world: World,
    control: PathBuf,
    _slot: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let slot = FIXTURE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let world = World::new();
        let control = world.home().join("direct");
        fixture::install(world.root(), &control);
        Self {
            world,
            control,
            _slot: slot,
        }
    }

    fn dots(&self) -> Command {
        let mut command = self.world.dots();
        command
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "dumb")
            .env("LC_ALL", "C")
            .env("DOTS_MISE_BIN", self.control.join("mise"))
            .env("DIRECT_DIR", &self.control)
            .env("DIRECT_EXE", env::current_exe().unwrap());
        command
    }

    fn identity(&self, role: &str) -> Identity {
        wait_for(role, || Identity::read(&self.control.join(role)))
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
}

impl<'a> Terminal<'a> {
    fn start(fixture: &'a Fixture, case: &str, tostop: bool) -> Self {
        let mut dots = fixture.dots();
        dots.env("DIRECT_CASE", case);
        Self::shell(fixture, dots, tostop, "\r\n")
    }

    fn shell(fixture: &'a Fixture, dots: Command, tostop: bool, newline: &'static str) -> Self {
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
        let mut shell = Command::new("/bin/bash");
        shell
            .args(["--noprofile", "--norc", "--noediting", "-i"])
            .env_clear()
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
                "PTY did not show {marker:?}, session processes (pid, group, stopped): {:?}, transcript:\n{}",
                self.session
                    .members()
                    .into_iter()
                    .map(|pid| (pid, group(pid), stopped(pid)))
                    .collect::<Vec<_>>(),
                String::from_utf8_lossy(&self.transcript)
            );
            let mut buffer = [0; 4096];
            match self.master.read(&mut buffer) {
                Ok(count) => self.transcript.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("reading PTY: {error}"),
            }
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
        let stage = fixture.identity("stage");
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
        let child = fixture.identity("stage");
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
    terminal.observe_foreground_group(&fixture.identity("stage"), None);
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
        terminal.observe_foreground_group(&fixture.identity("stage"), None);
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
    let stage = fixture.identity("stage");
    let grandchild = fixture.identity("grandchild");
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
    let stage = fixture.identity("stage");
    let grandchild = fixture.identity("grandchild");
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
    let stage = fixture.identity("stage");
    let grandchild = fixture.identity("grandchild");
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
    let stage = fixture.identity("stage");
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
        let stage = fixture.identity("stage");
        let grandchild = fixture.identity("grandchild");
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
