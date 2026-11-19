// These roles run in a separate invocation of the integration test executable.
// The normal test run enters the paired observation instead.
use super::unix::*;
use nix::libc;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicI32, Ordering::Relaxed};

pub const TEST_NAME: &str = "new_foreground_ctrl_c_observation_without_witness";
pub const PROMPT: &str = "native prompt> ";
pub const SUDO_PROMPT: &str = "stub sudo prompt> ";
pub const SUDO_INPUT: &str = "synthetic-tty-input";

const RUN: &str = r#"#!/bin/bash
set -eu
export DIRECT_CASE="$1"
printf '%s\n' "$DIRECT_CASE" > "$DIRECT_DIR/script-start"
shift
case "$DIRECT_CASE" in
    streams)
        IFS= read -r line
        printf 'stdin:[%s] argv:[%s][%s]\n' "$line" "$1" "$2"
        for (( i = 0; i < 4096; i++ )); do
            printf 'stdout\000\377abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXY0123456789\r\n'
            printf 'stderr\377\000ZYXWVUTSRQPONMLKJIHGFEDCBAzyxwvutsrqponmlkjihgfedcba9876543210\n' >&2
        done
        exit 23
        ;;
    sudo) exec "$DIRECT_DIR/sudo" ;;
    *) export DIRECT_ROLE=stage ;;
esac
exec "$DIRECT_EXE" --exact new_foreground_ctrl_c_observation_without_witness --nocapture
"#;

pub const OUT: &[u8] =
    b"stdout\0\xffabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXY0123456789\r\n";
pub const ERR: &[u8] =
    b"stderr\xff\0ZYXWVUTSRQPONMLKJIHGFEDCBAzyxwvutsrqponmlkjihgfedcba9876543210\n";

pub fn install(root: &Path, control: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let tests = root.join("tests");
    fs::create_dir(&tests).unwrap();
    fs::create_dir(control).unwrap();
    for (path, script) in [
        (tests.join("run"), RUN),
        (
            control.join("mise"),
            r#"#!/bin/sh
set -eu
: > "$DIRECT_DIR/mise-start"
if [ "$1" = -C ]; then cd "$2"; shift 2; fi
if [ "$1" = exec ] && [ "$2" = -- ]; then shift 2; exec "$@"; fi
printf 'unexpected fixture mise arguments: %s\n' "$*" >&2
exit 88
"#,
        ),
        (
            control.join("sudo"),
            r#"#!/bin/sh
DIRECT_ROLE=stage exec "$DIRECT_EXE" --exact new_foreground_ctrl_c_observation_without_witness --nocapture
"#,
        ),
    ] {
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[derive(Debug)]
pub struct Identity {
    pub pid: Pid,
    pub session: Pid,
    pub group: Pid,
    pub foreground: Pid,
    pub tty: Vec<String>,
    pub dev_tty_foreground: Pid,
}

impl Identity {
    pub fn read(path: &Path) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        let mut number = || lines.next().unwrap().parse::<Pid>().unwrap();
        Some(Self {
            pid: number(),
            session: number(),
            group: number(),
            foreground: number(),
            dev_tty_foreground: number(),
            tty: lines.map(str::to_string).collect(),
        })
    }
}

pub fn lock(path: &Path) -> bool {
    File::open(path).is_ok_and(|file| unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0
    })
}

fn register(dir: &Path, role: &str) -> File {
    let live = File::create(dir.join(format!("{role}.live"))).unwrap();
    unsafe { check(libc::flock(live.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)) }.unwrap();
    let pid = process::id() as Pid;
    let device = File::open("/dev/tty")
        .map(|file| foreground(file.as_raw_fd()))
        .unwrap_or(-1);
    let mut info = format!(
        "{}\n{}\n{}\n{}\n{}\n",
        pid,
        session(pid),
        group(pid),
        foreground(0),
        device
    );
    for fd in 0..=2 {
        info.push_str(&tty_name(fd).unwrap_or_else(|| "-".to_string()));
        info.push('\n');
    }
    fs::write(dir.join(format!("{role}.tmp")), info).unwrap();
    fs::rename(dir.join(format!("{role}.tmp")), dir.join(role)).unwrap();
    live
}

static INTERRUPTS: AtomicI32 = AtomicI32::new(0);
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

extern "C" fn count_interrupt(_: libc::c_int) {
    INTERRUPTS.fetch_add(1, Relaxed);
}

extern "C" fn exit_on_interrupt(_: libc::c_int) {
    let marker = b"native SIGINT handler\n";
    unsafe {
        libc::write(1, marker.as_ptr().cast(), marker.len());
        libc::_exit(EXIT_CODE.load(Relaxed));
    }
}

extern "C" fn continue_after_interrupt(_: libc::c_int) {
    let marker = b"native SIGINT ignored\n";
    unsafe { libc::write(1, marker.as_ptr().cast(), marker.len()) };
}

fn set_handler(signal: libc::c_int, handler: libc::sighandler_t) {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        check(libc::sigaction(signal, &action, std::ptr::null_mut())).unwrap();
    }
}

fn ignore_cancellation() {
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        set_handler(signal, libc::SIG_IGN);
    }
}

fn wait_forever() -> ! {
    loop {
        unsafe { libc::pause() };
    }
}

fn fixture_command(role: &str) -> Command {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env("DIRECT_ROLE", role);
    command
}

pub fn run_role(role: &str) -> ! {
    // Contain the fixture even if the test runner is interrupted during spawn.
    unsafe { libc::alarm(30) };
    let dir = PathBuf::from(env::var_os("DIRECT_DIR").unwrap());
    let case = env::var("DIRECT_CASE").unwrap();
    match role {
        "grandchild" => {
            if case != "suspend" {
                ignore_cancellation();
            }
            let _live = register(&dir, role);
            wait_forever()
        }
        "stage" => stage(&dir, &case),
        "observe" => observe(&dir),
        _ => panic!("unknown fixture role {role}"),
    }
}

fn stage(dir: &Path, case: &str) -> ! {
    match case {
        "trap-zero" | "trap-23" => {
            EXIT_CODE.store(if case == "trap-zero" { 0 } else { 23 }, Relaxed);
            set_handler(
                libc::SIGINT,
                exit_on_interrupt as *const () as libc::sighandler_t,
            );
        }
        "ignore-alone" | "ignoring" | "leader-exits" | "detached" => ignore_cancellation(),
        "suspend" => set_handler(libc::SIGTSTP, libc::SIG_DFL),
        "cancel-suspend-zero" | "cancel-suspend-23" => {
            set_handler(
                libc::SIGINT,
                continue_after_interrupt as *const () as libc::sighandler_t,
            );
            set_handler(libc::SIGTSTP, libc::SIG_DFL);
        }
        "background-reader" => set_handler(libc::SIGTTIN, libc::SIG_DFL),
        "line" | "background" | "background-stop" | "sudo" => {}
        _ => panic!("unknown fixture case {case}"),
    }
    let _live = register(dir, "stage");
    let grandchild = matches!(case, "ignoring" | "leader-exits" | "detached" | "suspend")
        .then(|| fixture_command("grandchild").spawn().unwrap());
    if grandchild.is_some() {
        wait_for("grandchild registration", || {
            Identity::read(&dir.join("grandchild"))
        });
    }
    match case {
        "detached" | "background" => wait_forever(),
        "background-stop" => {
            wait_for("permission to stop the background stage", || {
                dir.join("stop-stage").exists().then_some(())
            });
            check(unsafe { libc::raise(libc::SIGSTOP) }).unwrap();
            let _resumed = register(dir, "resumed");
            wait_for("permission to exit the continued stage", || {
                dir.join("finish-stage").exists().then_some(())
            });
            process::exit(0);
        }
        "background-reader" | "cancel-suspend-zero" | "cancel-suspend-23" => {
            let tty = File::open("/dev/tty").unwrap();
            let mut reader = io::BufReader::new(&tty);
            if case == "background-reader" {
                wait_for("permission to start the background tty read", || {
                    dir.join("read-tty").exists().then_some(())
                });
            }
            print!("{PROMPT}");
            io::stdout().flush().unwrap();
            if case != "background-reader" {
                read_reply(&mut reader, "still reading");
                println!("native input survived SIGINT");
            }
            read_reply(&mut reader, "ordinary input");
            assert_eq!(foreground(tty.as_raw_fd()), group(0));
            let _resumed = register(dir, "resumed");
            println!("ordinary input accepted");
            process::exit(if case == "cancel-suspend-23" { 23 } else { 0 });
        }
        "sudo" => {
            let mut tty = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .unwrap();
            let before = termios(tty.as_raw_fd());
            let mut quiet = before;
            quiet.c_lflag &= !libc::ECHO;
            set_termios(tty.as_raw_fd(), &quiet);
            write!(tty, "{SUDO_PROMPT}").unwrap();
            let mut line = String::new();
            let read = io::BufReader::new(&tty).read_line(&mut line);
            set_termios(tty.as_raw_fd(), &before);
            read.unwrap();
            assert_eq!(line.trim_end(), SUDO_INPUT);
            writeln!(tty, "stub sudo accepted").unwrap();
        }
        _ => {
            print!("{PROMPT}");
            io::stdout().flush().unwrap();
            let mut line = String::new();
            io::stdin().read_line(&mut line).unwrap();
            assert_eq!(line, "ordinary input\n");
            println!("ordinary input accepted");
        }
    }
    // Cancellation and normal-exit cleanup cases leave a live remnant for dots.
    if case == "suspend"
        && let Some(mut child) = grandchild
    {
        child.kill().unwrap();
        child.wait().unwrap();
    }
    process::exit(0)
}

fn read_reply(reader: &mut impl BufRead, reply: &str) {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.strip_suffix('\n'), Some(reply));
}

fn observe(dir: &Path) -> ! {
    set_handler(
        libc::SIGINT,
        count_interrupt as *const () as libc::sighandler_t,
    );
    set_handler(libc::SIGTTOU, libc::SIG_IGN);
    let mut command = fixture_command("stage");
    // Like dots in DIRECT, the supervisor hands off the original terminal
    // to a separate group. All other observations come from signals and wait.
    unsafe {
        command.pre_exec(|| {
            check(libc::setpgid(0, 0))?;
            check(libc::tcsetpgrp(0, libc::getpgrp()))
        });
    }
    let status = command.status().unwrap().code().unwrap();
    unsafe { check(libc::tcsetpgrp(0, libc::getpgrp())) }.unwrap();
    fs::write(
        dir.join("observation"),
        format!("{} {status}\n", INTERRUPTS.load(Relaxed)),
    )
    .unwrap();
    process::exit(status)
}
