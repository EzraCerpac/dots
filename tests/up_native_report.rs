mod support;
#[allow(dead_code)]
#[path = "direct_job_control/unix.rs"]
mod unix;

use nix::libc;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use support::World;
use unix::{acquire_controlling_tty, check, nonblocking, open_pty, set_termios, termios, tty_name};

const LABEL: &str = "Printer";
const PROMPT: &str = "fixture printer reply> ";
const REPLY: &str = "one ordinary fixture reply";
const HOOK_OUTPUT: &str = "fixture printer accepted the reply";
const REPORT_START: &str = "dots up: Failed (exit 1)\n";
const REPORT_FOOTER: &str = "Details and logs · dots up --last\n";

fn executable(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn prepare(world: &World) {
    let root = world.root();
    let bin = world.home().join("fixture-bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        root.join("config.toml"),
        "[vars]\ndots_profiles = \"laptop\"\n",
    )
    .unwrap();
    fs::write(root.join("dots/exceptions.tsv"), "").unwrap();
    let hooks = root.join("dots/hooks/update.d");
    for entry in fs::read_dir(&hooks).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    executable(
        &hooks.join("30-Printer"),
        br#"#!/bin/bash
set -eu
[[ -t 0 && -t 1 && -t 2 ]] || exit 97
[[ "$(tty)" == "$FIXTURE_TTY" ]] || exit 98
printf '%s' "$FIXTURE_PROMPT"
IFS= read -r reply </dev/tty
[[ "$reply" == "$FIXTURE_REPLY" ]] || exit 99
printf '%s\n' "$FIXTURE_HOOK_OUTPUT"
exit 23
"#,
    );
    // Reuse P-04's offline mise fixture. Its inventory response reaches the
    // production P-06 checkpoint and its call log proves the probe ran.
    let up_fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/up");
    for tool in ["mise", "brew"] {
        executable(&bin.join(tool), &fs::read(up_fixtures.join(tool)).unwrap());
    }
    executable(&bin.join("sudo"), b"#!/bin/sh\nexit 96\n");
}

fn dots(world: &World) -> support::FixtureCommand<'_> {
    let home = world.home();
    let bin = home.join("fixture-bin");
    let mut command = world.dots();
    for (key, relative) in [
        ("CODEX_HOME", ".codex"),
        ("CLAUDE_CONFIG_DIR", ".claude"),
        ("MISE_DATA_DIR", "mise/data"),
        ("MISE_CACHE_DIR", "mise/cache"),
        ("MISE_STATE_DIR", "mise/state"),
        ("MISE_TMP_DIR", "mise/tmp"),
        ("MISE_SYSTEM_CONFIG_DIR", "mise/system"),
        ("TMPDIR", "tmp"),
    ] {
        let dir = home.join(relative);
        fs::create_dir_all(&dir).unwrap();
        command.env(key, dir);
    }
    command
        .env(
            "PATH",
            std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                .unwrap(),
        )
        .env("TERM", "xterm-256color")
        .env("LC_ALL", "C")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("SETUP_PROFILE", "laptop")
        .env("SETUP_MACHINE_ID", "fixture")
        .env("DOTS_MISE_BIN", bin.join("mise"))
        .env("MISE_CONFIG_DIR", world.root())
        .env("MISE_TRUSTED_CONFIG_PATHS", world.root())
        .env("MISE_GLOBAL_CONFIG_FILE", home.join("mise/global.toml"))
        .env("MISE_SELF_UPDATE_AVAILABLE", "false");
    command
}

struct Session(Child);

impl Session {
    fn wait(&mut self) -> ExitStatus {
        unix::wait_for("dots to exit", || self.0.try_wait().unwrap())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let sid = self.0.id() as unix::Pid;
        for pid in unix::processes() {
            if pid > 1 && unix::session(pid) == sid {
                let _ = unix::signal(pid, libc::SIGKILL);
            }
        }
        let end = Instant::now() + Duration::from_secs(2);
        while self.0.try_wait().ok().flatten().is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn read_terminal(master: &mut File, transcript: &mut Vec<u8>) -> bool {
    let mut bytes = [0; 4096];
    match master.read(&mut bytes) {
        Ok(0) => true,
        Ok(n) => {
            transcript.extend_from_slice(&bytes[..n]);
            false
        }
        Err(error) if error.raw_os_error() == Some(libc::EIO) => true,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("reading private terminal: {error}"),
    }
}

fn prompted_update(world: &World) -> (ExitStatus, String) {
    let (mut master, slave) = open_pty();
    let mut modes = termios(slave.as_raw_fd());
    // Compare live and redirected report bytes with no CRLF processing.
    modes.c_oflag &= !libc::OPOST;
    modes.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
    set_termios(slave.as_raw_fd(), &modes);
    nonblocking(&master);
    let mut runner = dots(world);
    runner
        .arg("up")
        .env("FIXTURE_TTY", tty_name(slave.as_raw_fd()).unwrap())
        .env("FIXTURE_PROMPT", PROMPT)
        .env("FIXTURE_REPLY", REPLY)
        .env("FIXTURE_HOOK_OUTPUT", HOOK_OUTPUT)
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    unsafe {
        runner.pre_exec(|| {
            check(libc::setsid())?;
            acquire_controlling_tty()?;
            check(libc::tcsetpgrp(0, libc::getpgrp()))
        });
    }
    let mut job = Session(runner.spawn().unwrap());
    drop(runner);
    let mut transcript = Vec::new();
    unix::wait_for("the native printer prompt", || {
        let eof = read_terminal(&mut master, &mut transcript);
        let seen = transcript
            .windows(PROMPT.len())
            .any(|bytes| bytes == PROMPT.as_bytes());
        assert!(
            seen || !eof,
            "update closed its terminal before the prompt:\n{}",
            String::from_utf8_lossy(&transcript)
        );
        seen.then_some(())
    });
    let before_prompt = String::from_utf8_lossy(&transcript);
    let before_prompt = before_prompt.split_once(PROMPT).unwrap().0;
    assert!(
        before_prompt.ends_with(&format!("==> {LABEL}\n")),
        "{before_prompt}"
    );
    writeln!(master, "{REPLY}").unwrap();
    unix::wait_for("the completed update transcript", || {
        read_terminal(&mut master, &mut transcript).then_some(())
    });
    (job.wait(), String::from_utf8(transcript).unwrap())
}

fn check_state(dir: &Path, stage_log: &str) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let path = entry.path();
        if kind.is_dir() {
            check_state(&path, stage_log);
        } else if kind.is_file() {
            assert_ne!(
                entry.file_name(),
                stage_log,
                "retained a native stage log at {path:?}"
            );
            let bytes = fs::read(&path).unwrap();
            for marker in [PROMPT, REPLY, HOOK_OUTPUT] {
                assert!(
                    !bytes
                        .windows(marker.len())
                        .any(|window| window == marker.as_bytes()),
                    "retained native terminal bytes in {path:?}"
                );
            }
        }
    }
}

fn redirected_replay(world: &World) -> (ExitStatus, Vec<u8>) {
    let stdout = world.home().join("replay.stdout");
    let stderr = world.home().join("replay.stderr");
    let mut replay = dots(world);
    replay
        .args(["up", "--last"])
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    unsafe { replay.pre_exec(|| check(libc::setsid())) };
    let status = Session(replay.spawn().unwrap()).wait();
    let diagnostics = fs::read(stderr).unwrap();
    assert!(
        diagnostics.is_empty(),
        "{}",
        String::from_utf8_lossy(&diagnostics)
    );
    (status, fs::read(stdout).unwrap())
}

#[test]
fn new_up_replays_native_hook_failure_without_retaining_terminal_output() {
    let world = World::new();
    prepare(&world);
    let (status, live) = prompted_update(&world);
    assert_eq!(status.code(), Some(1), "{live}");
    assert!(live.contains(HOOK_OUTPUT), "{live}");
    let stage_result = format!("{LABEL}: failed");
    assert_eq!(
        live.lines().filter(|line| *line == stage_result).count(),
        1,
        "{live}"
    );
    assert_eq!(live.matches(REPORT_START).count(), 1, "{live}");
    let final_report = &live[live.find(REPORT_START).unwrap()..];
    assert!(
        final_report
            .lines()
            .any(|line| line == "    Exit status: 23"),
        "{final_report}"
    );
    assert!(
        final_report
            .lines()
            .any(|line| line == "    Output was shown on the terminal"),
        "{final_report}"
    );
    assert!(final_report.ends_with(REPORT_FOOTER), "{final_report}");

    let calls = fs::read_to_string(world.root().join("calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| *line == "checkpoint inventory")
            .count(),
        2,
        "{calls}"
    );
    assert!(
        !calls.lines().any(|line| line == "checkpoint save"),
        "{calls}"
    );
    for label in [
        "Pre-update settings checkpoint",
        "Post-update settings checkpoint",
    ] {
        assert!(
            live.lines().any(|line| line == format!("{label}: skipped")),
            "{live}"
        );
    }

    let state = world.home().join(".local/state/dots");
    let reports: Vec<_> = fs::read_dir(state.join("reports"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|file| file.extension().is_some_and(|ext| ext == "json"))
        .collect();
    assert_eq!(reports.len(), 1);
    let report: Value = serde_json::from_reader(File::open(&reports[0]).unwrap()).unwrap();
    assert_eq!(report["command"], "up");
    assert_eq!(report["status"], status.code().unwrap());
    assert_eq!(report["cancelled"], false);
    let stage = report["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["label"] == LABEL)
        .expect("saved Printer stage");
    assert_eq!(stage["outcome"], "failed");
    let issues = stage["issues"].as_array().unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["terminal_exit"], 23);
    assert!(issues[0]["tail"].is_null(), "{issues:?}");
    assert!(issues[0]["log"].is_null(), "{issues:?}");
    check_state(&state, &format!("{}.log", stage["id"].as_u64().unwrap()));

    let (replay_status, replay) = redirected_replay(&world);
    assert_eq!(replay_status.code(), status.code());
    assert_eq!(replay, final_report.as_bytes());
}
