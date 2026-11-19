use super::*;
use crate::Env;
use std::fs;
use std::path::Path;

mod detached_probe;

fn shell(source: &str, paths: &[&Path]) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args(["-c", source, "fixture"])
        .args(paths)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(TICK);
    }
    false
}

#[test]
fn cancellation_during_drain_ignores_exited_foreground_and_retires_other_groups() {
    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    let supervisor = Supervisor::new(&ctx, Mode::Direct);
    let heartbeat = fixture.path().join("draining");
    let alongside_ready = fixture.path().join("alongside-ready");

    let (result, foreground, alongside, ready) = std::thread::scope(|scope| {
        let foreground = scope.spawn(|| {
            let command = shell(
                r#"(trap '' INT TERM
                    i=0
                    while [ "$i" -lt 250 ]; do
                        printf . >> "$1"
                        sleep .04
                        i=$((i+1))
                    done) &
                    exit 23"#,
                &[&heartbeat],
            );
            let mut stage = supervisor
                .spawn(command, Placement::HeadlessForeground, false)
                .unwrap();
            Pipes::take(&mut stage.child)
                .unwrap()
                .collect(&mut stage)
                .unwrap()
                .status
        });
        let alongside = scope.spawn(|| {
            let command = shell(
                r#"trap 'exit 47' INT
                    trap '' TERM
                    : > "$1"
                    i=0
                    while [ "$i" -lt 500 ]; do
                        sleep .02
                        i=$((i+1))
                    done"#,
                &[&alongside_ready],
            );
            let mut stage = supervisor.spawn(command, Placement::Probe, false).unwrap();
            Pipes::take(&mut stage.child)
                .unwrap()
                .collect(&mut stage)
                .unwrap()
                .status
        });
        let ready = wait_until(|| {
            heartbeat.exists()
                && alongside_ready.exists()
                && supervisor.state().children.iter().any(|entry| {
                    entry.foreground && matches!(observe(entry.pid, false), Ok(Observed::Exited))
                })
        });
        supervisor.state().cancel(Some(libc::SIGINT), None);
        let denied = supervisor.admit_stage(|| panic!("admitted a stage after cancellation"));
        assert_eq!(denied, None);
        assert_eq!(
            supervisor
                .probe(Command::new("/bin/false"), Stdio::null(), false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted,
        );
        let result = supervisor.complete(Some(ReportCommand::Up), 1, || {});
        assert!(supervisor.state().children.is_empty());
        (
            result,
            foreground.join().unwrap(),
            alongside.join().unwrap(),
            ready,
        )
    });

    assert!(
        ready,
        "foreground did not enter drain with an active alongside stage"
    );
    assert_eq!(foreground.code(), Some(23));
    assert_eq!(alongside.code(), Some(47), "{alongside:?}");
    assert!(result.cancelled);
    assert_eq!(result.status, 130);

    // SIGKILL has retired the stubborn descendant, even though its original
    // leader had exited before the signal. A zombie cannot write a heartbeat.
    let size_after_cleanup = fs::metadata(&heartbeat).unwrap().len();
    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(fs::metadata(&heartbeat).unwrap().len(), size_after_cleanup);
}

#[test]
fn active_foreground_status_wins_over_fallback_and_sync_remains_zero() {
    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    let supervisor = Supervisor::new(&ctx, Mode::Direct);
    let ready_path = fixture.path().join("foreground-ready");
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let command = shell(
                r#"trap 'exit 23' INT
                    trap '' TERM
                    : > "$1"
                    i=0
                    while [ "$i" -lt 500 ]; do
                        sleep .02
                        i=$((i+1))
                    done"#,
                &[&ready_path],
            );
            let mut stage = supervisor
                .spawn(command, Placement::HeadlessForeground, false)
                .unwrap();
            stage.wait().unwrap()
        });
        let ready = wait_until(|| ready_path.exists());
        supervisor.state().cancel(Some(libc::SIGINT), None);
        let result = supervisor.complete(Some(ReportCommand::Up), 1, || {});
        assert!(ready, "foreground did not register its interrupt trap");
        let native = worker.join().unwrap();
        assert_eq!(native.code(), Some(23), "{native:?}");
        assert!(result.cancelled);
        assert_eq!(result.status, 23);
        let sync = supervisor.complete(Some(ReportCommand::Sync), 1, || {});
        assert!(sync.cancelled);
        assert_eq!(sync.status, 0);
    });
}

fn fixture_context(path: &Path, verbose: bool) -> crate::Ctx {
    let env = Env {
        vars: PRIVATE_DIRS
            .iter()
            .map(|name| ((*name).into(), path.as_os_str().into()))
            .chain([("DOTS_ROOT".into(), path.as_os_str().into())])
            .collect(),
        cwd: path.to_owned(),
    };
    let root = crate::config::Root::resolve(&env).unwrap();
    crate::Ctx {
        env,
        root,
        flags: crate::Flags {
            verbose,
            ..crate::Flags::default()
        },
    }
}

#[test]
fn alongside_stage_and_probe_have_no_controlling_tty_or_input() {
    const FIXTURE: &str = "DOTS_SUPERVISOR_PIPE_FIXTURE";
    const INPUT: &[u8] = b"unconsumed caller input\n";

    if let Some(path) = std::env::var_os(FIXTURE) {
        check_alongside_session(Path::new(&path));
        let mut input = Vec::new();
        std::io::Read::read_to_end(&mut io::stdin().lock(), &mut input).unwrap();
        assert_eq!(input, INPUT);
        return;
    }

    let fixture = tempfile::tempdir().unwrap();
    let output = fixture_process(
        fixture.path(),
        FIXTURE,
        "supervisor::tests::alongside_stage_and_probe_have_no_controlling_tty_or_input",
        INPUT,
    );
    assert!(output.status.success(), "{output:?}");
}

fn fixture_process(path: &Path, flag: &str, test: &str, input: &[u8]) -> Output {
    let mut child = fixture_command(path, flag, test).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

const PRIVATE_DIRS: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
    "TMPDIR",
    "TMP",
    "TEMP",
    "MISE_CONFIG_DIR",
    "MISE_SYSTEM_CONFIG_DIR",
    "MISE_CACHE_DIR",
    "MISE_DATA_DIR",
    "MISE_STATE_DIR",
    "MISE_TMP_DIR",
    "MISE_INSTALLS_DIR",
    "MISE_INSTALL_STORE_DIR",
    "MISE_DOWNLOADS_DIR",
    "MISE_PLUGINS_DIR",
    "MISE_SHIMS_DIR",
    "MISE_SYSTEM_DATA_DIR",
    "MISE_SYSTEM_INSTALLS_DIR",
    "MISE_SYSTEM_SHIMS_DIR",
    "MISE_SYSTEM_PLUGINS_DIRS",
];

fn fixture_command(path: &Path, flag: &str, test: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .env(flag, path)
        .env("PATH", "/usr/bin:/bin")
        .args(["--exact", test, "--nocapture"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in PRIVATE_DIRS {
        command.env(name, path);
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

fn check_alongside_session(path: &Path) {
    use crate::run::{Run, Sink};

    let ctx = fixture_context(path, false);
    let run = Run::begin(&ctx, Sink::Unreported);
    let id = run.start_alongside("Fixture");

    for probe in [false, true] {
        let registered = path.join("session-pid");
        let release = path.join("release");
        let mut command = shell(
            r#"if (/bin/true </dev/tty) 2>/dev/null; then exit 21; fi
                if IFS= read -r line; then exit 22; fi
                printf %s "$$" > "$1"
                i=0
                while [ ! -e "$2" ] && [ "$i" -lt 250 ]; do
                    sleep .04
                    i=$((i+1))
                done
                test -e "$2""#,
            &[&registered, &release],
        );
        command.stdin(Stdio::inherit());
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                if probe {
                    run.probe_for(id, command).unwrap().status
                } else {
                    run.run(id, command, Policy::Ordinary).unwrap()
                }
            });
            let mut pid = None;
            let ready = wait_until(|| {
                pid = fs::read_to_string(&registered)
                    .ok()
                    .and_then(|text| text.parse::<libc::pid_t>().ok());
                pid.is_some()
            });
            let sid = pid.map(|pid| unsafe { libc::getsid(pid) });
            fs::write(&release, b"").unwrap();
            let status = worker.join().unwrap();
            assert!(ready, "stage did not register: {status}");
            assert!(status.success(), "stage failed: {status}");
            assert_eq!(sid, pid, "stage should lead a new session");
        });
        fs::remove_file(registered).unwrap();
        fs::remove_file(release).unwrap();
    }
    assert_eq!(run.end(0), 0);
}

#[test]
fn piped_update_captures_may_prompt_output_and_keeps_caller_input() {
    use crate::run::{Outcome, Run, Sink};

    const QUIET: &str = "DOTS_SUPERVISOR_PIPED_UP";
    const VERBOSE: &str = "DOTS_SUPERVISOR_VERBOSE_UP";
    const OUT: &str = "fixture stdout: caller input";
    const ERR: &str = "fixture stderr";
    const SKIPPED: &str = "Sync configuration (no remote configured)";
    if let Some((path, verbose)) = std::env::var_os(QUIET)
        .map(|path| (path, false))
        .or_else(|| std::env::var_os(VERBOSE).map(|path| (path, true)))
    {
        let ctx = fixture_context(Path::new(&path), verbose);
        let run = Run::begin(&ctx, Sink::Report(ReportCommand::Up));
        run.skip(SKIPPED);
        let stage = run.start("May prompt");
        let command = shell(
            r#"IFS= read -r line || exit 9
                printf 'fixture stdout: %s\n' "$line"
                printf 'fixture stderr\n' >&2
                exit 23"#,
            &[],
        );
        let status = run.run(stage, command, Policy::Interactive).unwrap();
        assert_eq!(status.code(), Some(23));
        run.issue(stage, Issue::new("Fixture failed.", "Retry the fixture."));
        run.finish(stage, Outcome::Failed);
        assert_eq!(run.end(1), 1);
        return;
    }

    for (flag, verbose) in [(QUIET, false), (VERBOSE, true)] {
        let fixture = tempfile::tempdir().unwrap();
        let output = fixture_process(
            fixture.path(),
            flag,
            "supervisor::tests::piped_update_captures_may_prompt_output_and_keeps_caller_input",
            b"caller input\n",
        );
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stdout.lines().any(|line| line == OUT), verbose, "{stdout}");
        assert_eq!(stderr.lines().any(|line| line == ERR), verbose, "{stderr}");
        assert!(!stdout.contains("Output was shown on the terminal"));
        assert_eq!(stdout.matches(SKIPPED).count(), 1, "{stdout}");

        let ctx = fixture_context(fixture.path(), verbose);
        let entries = crate::report::list(&ctx.env, Some(ReportCommand::Up)).unwrap();
        let report = crate::report::load(&ctx.env, &entries[0].id).unwrap();
        assert_eq!(report.stages[0].outcome, Some(Outcome::Skipped));
        let issue = &report.stages[1].issues[0];
        assert!(issue.terminal_exit.is_none());
        let tail = issue.tail.as_deref().unwrap();
        let log = fs::read_to_string(issue.log.as_ref().unwrap()).unwrap();
        for line in [OUT, ERR] {
            assert!(tail.lines().any(|entry| entry == line), "{tail}");
            assert!(log.lines().any(|entry| entry == line), "{log}");
        }
    }
}

#[test]
fn probe_with_private_input_does_not_enrich_reports_or_transcripts() {
    use crate::run::{Outcome, Run, Sink};
    use std::io::{Seek, SeekFrom};

    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), true);
    let run = Run::begin(&ctx, Sink::Report(ReportCommand::Up));
    let stage = run.start("Private input check");
    let secret = b"synthetic probe input and output\n";
    let mut file = tempfile::tempfile_in(fixture.path()).unwrap();
    file.write_all(secret).unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();

    let output = run
        .probe_with_stdin(Command::new("/bin/cat"), Stdio::from(file))
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, secret);
    assert!(output.stderr.is_empty());
    run.issue(stage, Issue::new("Probe deferred.", "Retry the probe."));
    run.finish(stage, Outcome::Deferred);
    assert_eq!(run.end(0), 0);

    let entries = crate::report::list(&ctx.env, Some(ReportCommand::Up)).unwrap();
    let report = crate::report::load(&ctx.env, &entries[0].id).unwrap();
    let issue = &report.stages[0].issues[0];
    assert!(issue.tail.is_none());
    assert!(issue.log.is_none());
    assert!(!fixture.path().join("dots/logs").exists());
    let mut transcript = Vec::new();
    crate::ui::transcript::write_report(&mut transcript, &report, true).unwrap();
    assert!(
        !transcript
            .windows(secret.len() - 1)
            .any(|bytes| bytes == &secret[..secret.len() - 1])
    );
}

#[test]
fn report_retention_uses_its_log_id_instead_of_an_issue_path() {
    use crate::report::{self, Report, Stage};
    use crate::run::Outcome;
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    let state = ctx.env.state_dir();
    let alternate = fixture.path().join("unrelated");
    fs::write(&alternate, "keep me").unwrap();
    let mut active = Logs::default();
    let mut active_log = active.stage(&state, StageId(0));
    active_log.record(b"still active");
    let active_id = active.id().unwrap();
    let mut saved_ids = Vec::new();

    for timestamp in 0..=10 {
        let mut logs = Logs::default();
        let mut log = logs.stage(&state, StageId(0));
        log.record(b"captured output");
        logs.store(StageId(0), log);
        let log_id = logs.id().unwrap();
        let mut issue = Issue::new("Fixture issue.", "Retry.");
        issue.log = Some(alternate.clone());
        let report = Report {
            command: ReportCommand::Up,
            finished_at_ms: timestamp,
            status: 1,
            cancelled: false,
            next: None,
            stages: vec![Stage {
                id: StageId(0),
                label: "Fixture".into(),
                alongside: false,
                outcome: Some(Outcome::Failed),
                changes: Vec::new(),
                issues: vec![issue],
            }],
        };
        report::save(&ctx.env, &report, Some(&log_id)).unwrap();
        logs.keep();
        saved_ids.push(log_id);
    }

    let logs_root = state.unwrap().join("logs");
    let oldest_logs = logs_root.join(&saved_ids[0]);
    fs::set_permissions(&oldest_logs, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        report::prune(&ctx.env, ReportCommand::Up)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let retained = report::list(&ctx.env, Some(ReportCommand::Up)).unwrap();
    assert_eq!(retained.len(), 11);
    assert_eq!(retained.last().unwrap().finished_at_ms, 0);
    assert!(oldest_logs.join("0.log").is_file());
    fs::set_permissions(&oldest_logs, fs::Permissions::from_mode(0o700)).unwrap();

    report::prune(&ctx.env, ReportCommand::Up).unwrap();
    let retained = report::list(&ctx.env, Some(ReportCommand::Up)).unwrap();
    assert_eq!(retained.len(), 10);
    assert_eq!(retained.last().unwrap().finished_at_ms, 1);
    assert!(!logs_root.join(&saved_ids[0]).exists());
    assert!(logs_root.join(&active_id).join("0.log").is_file());
    assert!(
        saved_ids[1..]
            .iter()
            .all(|id| logs_root.join(id).join("0.log").is_file())
    );
    assert_eq!(fs::read_to_string(alternate).unwrap(), "keep me");
}

#[test]
fn delayed_owned_probe_output_is_fully_collected() {
    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    let supervisor = Supervisor::new(&ctx, Mode::Direct);
    let command = shell(
        r#"(sleep 1.2
            printf 'delayed stdout'
            printf 'delayed stderr' >&2) &
            exit 0"#,
        &[],
    );
    let output = supervisor.probe(command, Stdio::null(), false).unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"delayed stdout");
    assert_eq!(output.stderr, b"delayed stderr");
}

#[test]
fn empty_group_does_not_discard_already_buffered_probe_output() {
    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    let supervisor = Supervisor::new(&ctx, Mode::Direct);
    let mut child = supervisor
        .spawn(
            shell("printf out; printf err >&2", &[]),
            Placement::Probe,
            false,
        )
        .unwrap();
    assert!(wait_until(|| matches!(
        observe(child.entry.pid, false),
        Ok(Observed::Exited)
    )));
    // Let the leader/group be gone before this pump sees their final bytes.
    let idle_since = Instant::now() - Duration::from_secs(2);
    child.last_output = idle_since;
    child.leader_exit = Some(idle_since);
    let output = Pipes::take(&mut child.child)
        .unwrap()
        .collect(&mut child)
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"out");
    assert_eq!(output.stderr, b"err");
}

#[test]
fn captured_foreground_interrupt_escalates_against_ignoring_leader() {
    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    let supervisor = Supervisor::new(&ctx, Mode::Direct);
    let ready = fixture.path().join("registered");
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let command = shell(
                r#"trap '' INT TERM
                    : > "$1"
                    i=0
                    while [ "$i" -lt 250 ]; do
                        sleep .04
                        i=$((i+1))
                    done"#,
                &[&ready],
            );
            let mut stage = supervisor
                .spawn(command, Placement::HeadlessForeground, false)
                .unwrap();
            Pipes::take(&mut stage.child)
                .unwrap()
                .collect(&mut stage)
                .unwrap()
                .status
        });
        let registered = wait_until(|| ready.exists());
        {
            let mut state = supervisor.state();
            let entry = *state.children.first().unwrap();
            state.terminal_interrupt(entry, false);
        }
        let result = supervisor.complete(Some(ReportCommand::Up), 1, || {});
        assert!(registered);
        let exit = worker.join().unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&exit),
            Some(libc::SIGKILL)
        );
        assert!(result.cancelled);
        assert_eq!(result.status, 130);
    });
}

#[test]
fn cancellation_finishes_report_during_blocked_verbose_boundary() {
    use crate::run::{Outcome, Run, Sink};

    const FLAG: &str = "DOTS_SUPERVISOR_BLOCKED_BOUNDARY";
    if let Some(path) = std::env::var_os(FLAG) {
        let path = Path::new(&path);
        let ctx = fixture_context(path, true);
        let run = Run::begin(&ctx, Sink::Report(ReportCommand::Up));
        let stage = run.start_alongside("Verbose worker");
        let command = shell("head -c 2097152 /dev/zero", &[]);
        assert!(run.run(stage, command, Policy::Ordinary).unwrap().success());
        run.finish(stage, Outcome::Completed);
        fs::write(path.join("replaying"), "").unwrap();
        assert_eq!(run.end(0), 130);
        return;
    }

    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path();
    let ctx = fixture_context(path, true);
    let mut child = fixture_command(
        path,
        FLAG,
        "supervisor::tests::cancellation_finishes_report_during_blocked_verbose_boundary",
    )
    .stdin(Stdio::null())
    .spawn()
    .unwrap();
    let ready = wait_until(|| path.join("replaying").exists());
    let pending = std::os::fd::AsRawFd::as_raw_fd(child.stdout.as_ref().unwrap());
    let ready = ready && pipe::ready(pending, libc::POLLIN, Duration::from_secs(1)).unwrap();
    let absent_before_signal = crate::report::list(&ctx.env, Some(ReportCommand::Up))
        .unwrap()
        .is_empty();
    let interrupted = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) } == 0;
    let saved = wait_until(|| {
        !crate::report::list(&ctx.env, Some(ReportCommand::Up))
            .unwrap()
            .is_empty()
    });
    if !saved {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        ready && absent_before_signal && interrupted && saved,
        "expected the report while the output pipe was still blocked; status {}",
        output.status
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let entry = &crate::report::list(&ctx.env, Some(ReportCommand::Up)).unwrap()[0];
    let report = crate::report::load(&ctx.env, &entry.id).unwrap();
    assert!(report.cancelled);
    assert_eq!(report.status, 130);
    assert_eq!(report.stages[0].outcome, Some(Outcome::Completed));
}

#[test]
fn child_sigint_is_a_status_in_owned_and_fragment_runs() {
    use std::os::unix::process::ExitStatusExt;

    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    for mode in [Mode::Direct, Mode::Fragment] {
        let supervisor = Supervisor::new(&ctx, mode);
        let source = "kill -INT \"$$\"";
        let output = supervisor
            .probe(shell(source, &[]), Stdio::null(), false)
            .unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGINT));
        let mut stage = supervisor
            .spawn(shell(source, &[]), Placement::HeadlessForeground, true)
            .unwrap();
        assert_eq!(stage.wait().unwrap().signal(), Some(libc::SIGINT));
        drop(stage);
        assert!(!supervisor.cancelled());
        assert!(supervisor.admit_stage(|| StageId(0)).is_some());
    }
}

#[test]
fn drop_verifies_membership_even_after_synthetic_leader_refusal() {
    use std::sync::atomic::Ordering;

    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    let supervisor = Supervisor::new(&ctx, Mode::Direct);
    let mut stage = supervisor
        .spawn(shell("exec sleep 10", &[]), Placement::Probe, false)
        .unwrap();
    let pid = stage.entry.pid;
    let diagnostic = "dots: synthetic SIGKILL refusal for the leader";
    supervisor.state().warnings.push(diagnostic.into());
    // The owned group is real and still receives SIGKILL. Simulate the
    // separate positive-PID refusal without creating a privileged process.
    stage.leader_kill = Some(false);
    drop(stage);

    assert!(supervisor.membership_checks.load(Ordering::Relaxed) > 0);
    assert!(
        supervisor
            .state()
            .warnings
            .iter()
            .any(|warning| warning == diagnostic)
    );
    assert!(supervisor.state().children.is_empty());
    assert!(!groups::has_live(pid, None).unwrap());
}

#[test]
fn failed_pump_keeps_tail_without_advertising_log() {
    use crate::run::{Outcome, Run, Sink};
    use std::os::fd::AsRawFd;

    const FLAG: &str = "DOTS_SUPERVISOR_PUMP_FAILURE";
    if let Some(path) = std::env::var_os(FLAG) {
        let ctx = fixture_context(Path::new(&path), true);
        let dev_null = File::open("/dev/null").unwrap();
        // Make an actual verbose write fail after capture has stored bytes.
        assert_eq!(
            unsafe { libc::dup2(dev_null.as_raw_fd(), libc::STDERR_FILENO) },
            libc::STDERR_FILENO
        );
        let run = Run::begin(&ctx, Sink::Report(ReportCommand::Up));
        let id = run.start("Captured step");
        let error = run
            .run(
                id,
                shell("printf 'useful tail\\n' >&2", &[]),
                Policy::Ordinary,
            )
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        run.issue(id, Issue::new("Fixture output failed.", "Retry."));
        run.finish(id, Outcome::Failed);
        assert_eq!(run.end(1), 1);
        return;
    }

    let fixture = tempfile::tempdir().unwrap();
    let output = fixture_process(
        fixture.path(),
        FLAG,
        "supervisor::tests::failed_pump_keeps_tail_without_advertising_log",
        b"",
    );
    assert!(output.status.success(), "{output:?}");
    let ctx = fixture_context(fixture.path(), true);
    let entry = &crate::report::list(&ctx.env, Some(ReportCommand::Up)).unwrap()[0];
    let report = crate::report::load(&ctx.env, &entry.id).unwrap();
    let issue = &report.stages[0].issues[0];
    assert_eq!(issue.tail.as_deref(), Some("useful tail"));
    assert!(issue.log.is_none());
}

#[test]
fn incomplete_alongside_log_replays_recorded_bytes_without_a_full_log_link() {
    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), true);
    let supervisor = Supervisor::new(&ctx, Mode::up(&ctx));
    let id = StageId(0);
    let mut logs = supervisor.logs.lock().unwrap();
    let mut diagnostic = logs.stage(&supervisor.log_base, id);
    diagnostic.record(b"recorded bytes");
    diagnostic.incomplete();
    diagnostic.record(b"\nremaining tail\n");
    logs.store(id, diagnostic);
    drop(logs);

    let mut replay = Vec::new();
    supervisor.replay_alongside(id, &mut replay).unwrap();
    assert_eq!(replay, b"recorded bytes\n");
    let mut issue = Issue::new("Capture stopped", "Retry it");
    supervisor.enrich_issue(id, &mut issue);
    assert!(issue.log.is_none());
    assert_eq!(
        issue.tail.as_deref(),
        Some("recorded bytes\nremaining tail")
    );
}

#[test]
fn tty_probe_uses_pinned_observer_and_respects_foreground_ownership() {
    run_tty_probe_fixture(
        "supervisor::tests::tty_probe_uses_pinned_observer_and_respects_foreground_ownership",
        false,
    );
}

#[test]
fn tty_probe_resumes_helper_when_launcher_catches_ttin() {
    run_tty_probe_fixture(
        "supervisor::tests::tty_probe_resumes_helper_when_launcher_catches_ttin",
        true,
    );
}

fn run_tty_probe_fixture(test: &str, helper: bool) {
    use std::io::Read;
    use std::os::fd::FromRawFd;

    const FLAG: &str = "DOTS_SUPERVISOR_TTY_PROBE";
    if let Some(path) = std::env::var_os(FLAG) {
        check_tty_probe(Path::new(&path), helper);
        return;
    }

    let fixture = tempfile::tempdir().unwrap();
    let (mut master, mut slave) = (-1, -1);
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let mut outer = unsafe { File::from_raw_fd(master) };
    let inner = unsafe { File::from_raw_fd(slave) };
    for fd in [master, slave] {
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
    }
    pipe::nonblocking(master).unwrap();

    let mut child = unsafe {
        fixture_command(fixture.path(), FLAG, test)
            .stdin(inner.try_clone().unwrap())
            .stdout(inner.try_clone().unwrap())
            .stderr(inner)
            .pre_exec(|| {
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1
                    || libc::tcsetpgrp(0, libc::getpgrp()) == -1
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })
            .spawn()
            .unwrap()
    };

    let mut transcript = Vec::new();
    let drain = |outer: &mut File, transcript: &mut Vec<u8>| {
        let mut bytes = [0; 4096];
        while let Ok(n @ 1..) = outer.read(&mut bytes) {
            transcript.extend_from_slice(&bytes[..n]);
        }
    };
    let prompted = wait_until(|| {
        drain(&mut outer, &mut transcript);
        transcript
            .windows(b"probe prompt".len())
            .any(|bytes| bytes == b"probe prompt")
    });
    if prompted {
        outer.write_all(b"tty answer\n").unwrap();
    }
    let mut result = None;
    let exited = prompted
        && wait_until(|| {
            drain(&mut outer, &mut transcript);
            result = child.try_wait().unwrap();
            result.is_some()
        });
    drain(&mut outer, &mut transcript);
    let foreground = unsafe { libc::tcgetpgrp(master) };
    let phase = fs::read_to_string(fixture.path().join("probe-phase")).unwrap_or_default();
    let observer = fs::read_to_string(fixture.path().join("observer-used")).unwrap_or_default();
    drop(outer);
    if !exited {
        let _ = child.kill();
        let _ = wait_until(|| {
            result = child.try_wait().unwrap();
            result.is_some()
        });
    }
    assert!(
        exited && result.is_some_and(|status| status.success()),
        "fixture PID {}, status {result:?}, foreground {foreground}, phase {phase:?}, observer {observer:?}\n{}",
        child.id(),
        String::from_utf8_lossy(&transcript)
    );
    if let Ok(evidence) = fs::read_to_string(fixture.path().join("busybox-poll")) {
        let cleanup = fs::read_to_string(fixture.path().join("competing-cleanup")).unwrap();
        writeln!(io::stderr().lock(), "{evidence}\n{cleanup}").unwrap();
    }
}

fn check_tty_probe(path: &Path, helper: bool) {
    use std::io::{Seek, SeekFrom};
    use std::os::unix::fs::PermissionsExt;

    let mut ctx = fixture_context(path, false);
    let observed = path.join("observer-used");
    ctx.env
        .vars
        .insert("FIXTURE_TRACE".into(), observed.clone().into());
    // Model a prepared Ctx pin with a tiny observer. The test executable has
    // no observer entry point, so using current_exe here fails the handshake.
    let bin = tempfile::tempdir_in(path).unwrap();
    let pinned = bin.path().join("dots");
    fs::write(
        &pinned,
        r#"#!/bin/sh
PATH=/usr/bin:/bin
printf 'ready\n' >> "$FIXTURE_TRACE"
printf R
marker=$(dd bs=1 count=1 2>/dev/null)
printf 'dd status %s, marker %s\n' "$?" "$marker" >> "$FIXTURE_TRACE"
if [ "$marker" = F ]; then printf F; fi
"#,
    )
    .unwrap();
    fs::set_permissions(&pinned, fs::Permissions::from_mode(0o700)).unwrap();
    ctx.root.child_bin.set(Ok(bin)).unwrap();

    let supervisor = Supervisor::new(&ctx, Mode::Direct);
    assert!(
        supervisor
            .probe(shell(":", &[]), Stdio::null(), false)
            .unwrap()
            .status
            .success()
    );
    assert!(!observed.exists(), "quiet probe started an observer");

    let mut input = tempfile::tempfile_in(path).unwrap();
    input.write_all(b"private stdin\n").unwrap();
    input.seek(SeekFrom::Start(0)).unwrap();
    let phase = path.join("probe-phase");
    let reader = r#"printf 'probe prompt\n' >/dev/tty
            printf 'reading tty\n' > "$1"
            IFS= read -r answer </dev/tty || exit 31
            printf 'reading private input\n' > "$1"
            IFS= read -r private || exit 32
            printf '%s:%s' "$answer" "$private"
            printf 'returning\n' > "$1""#;
    let trap_marker = path.join("launcher-caught-ttin");
    let command = if helper {
        shell(
            r#"trap ': > "$3"' TTIN
            exec 3<&0
            /bin/sh -c "$1" reader "$2" <&3 &
            reader=$!
            wait "$reader"
            code=$?
            if [ "$code" -gt 128 ]; then wait "$reader"; else exit "$code"; fi"#,
            &[Path::new(reader), &phase, &trap_marker],
        )
    } else {
        shell(reader, &[&phase])
    };
    let output = supervisor
        .probe(command, Stdio::from(input), false)
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"tty answer:private stdin");
    assert!(observed.is_file());
    if helper {
        assert!(
            trap_marker.is_file(),
            "launcher did not catch terminal stop signal"
        );
        return;
    }

    fs::write(&phase, "checking competing foreground").unwrap();
    let ready = path.join("foreground");
    std::thread::scope(|scope| {
        let (reader, mut release) = io::pipe().unwrap();
        let ready = &ready;
        let supervisor = &supervisor;
        let worker = scope.spawn(move || {
            let mut command = shell("IFS= read -r line && test \"$line\" = release", &[]);
            command.stdin(reader);
            let mut stage = supervisor
                .spawn(command, Placement::Foreground, true)
                .unwrap();
            assert!(
                stage
                    .terminal
                    .as_ref()
                    .is_some_and(TerminalJob::owns_foreground)
            );
            fs::write(ready, stage.terminal.as_ref().unwrap().group().to_string()).unwrap();
            stage.wait().unwrap()
        });
        let mut foreground = None;
        assert!(wait_until(|| {
            foreground = fs::read_to_string(ready)
                .ok()
                .and_then(|text| text.parse::<libc::pid_t>().ok());
            foreground.is_some()
        }));
        let foreground = foreground.unwrap();
        #[cfg(target_os = "linux")]
        check_busybox_poll(path, supervisor, foreground);
        // BusyBox's read builtin polls for data first. dd enters read(2) with
        // the tty input queue empty, so the kernel stops the competing group.
        let competing_pid = path.join("competing-pid");
        let competing = shell(
            "printf '%s' \"$$\" > \"$2\"; printf 'competing reader on tty\\n' > \"$1\"; exec dd if=/dev/tty bs=1 count=1",
            &[&phase, &competing_pid],
        );
        let outcome = supervisor.probe(competing, Stdio::null(), false);
        fs::write(&phase, format!("competing probe returned {outcome:?}")).unwrap();
        let error = outcome.unwrap_err();
        assert!(
            error.to_string().contains("another foreground command"),
            "{error}"
        );
        let pid: libc::pid_t = fs::read_to_string(competing_pid).unwrap().parse().unwrap();
        assert!(
            matches!(observe(pid, false), Err(error) if error.raw_os_error() == Some(libc::ECHILD))
        );
        assert!(
            supervisor
                .state()
                .children
                .iter()
                .all(|child| child.pid != pid)
        );
        let tty = terminal::controlling_terminal().unwrap();
        use std::os::fd::AsRawFd;
        assert_eq!(unsafe { libc::tcgetpgrp(tty.as_raw_fd()) }, foreground);
        fs::write(path.join("competing-cleanup"), format!("Competing dd PID {pid} was refused and reaped; foreground group {foreground} retained ownership.")).unwrap();
        release.write_all(b"release\n").unwrap();
        let status = worker.join().unwrap();
        assert!(status.success(), "{status}");
    });

    fs::write(&phase, "checking termios reclaim").unwrap();
    check_terminal_reclaim(&ctx, &supervisor);
    fs::write(&phase, "complete").unwrap();
}

#[cfg(target_os = "linux")]
fn check_busybox_poll(path: &Path, supervisor: &Supervisor<'_>, foreground: libc::pid_t) {
    if !fs::canonicalize("/bin/sh").is_ok_and(|path| path.ends_with("busybox")) {
        return;
    }
    let ready = path.join("ash-waiting");
    let probe = supervisor
        .spawn(
            shell(
                "printf ready > \"$1\"; IFS= read -r line </dev/tty",
                &[&ready],
            ),
            Placement::Probe,
            false,
        )
        .unwrap();
    let pid = probe.entry.pid;
    let pgid = unsafe { libc::getpgid(pid) };
    assert_eq!(pgid, pid);
    assert_ne!(pgid, foreground);
    let mut tasks = None;
    assert!(
        wait_until(|| {
            if ready.is_file() {
                tasks = sleeping_threads(pid);
            }
            tasks.is_some()
        }),
        "BusyBox read did not reach its empty-input wait"
    );
    assert!(matches!(observe(pid, true), Ok(Observed::Running)));
    fs::write(
        path.join("busybox-poll"),
        format!(
            "BusyBox ash PID {pid}, process group {pgid}, foreground {foreground}: {}",
            tasks.unwrap()
        ),
    )
    .unwrap();
    drop(probe);
}

#[cfg(target_os = "linux")]
fn sleeping_threads(pid: libc::pid_t) -> Option<String> {
    let mut evidence = Vec::new();
    for task in fs::read_dir(format!("/proc/{pid}/task")).ok()? {
        let path = task.ok()?.path();
        let status = fs::read_to_string(path.join("status")).ok()?;
        let field = |key| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .map(str::trim)
        };
        let state = field("State:")?;
        if !state.starts_with("S ") {
            return None;
        }
        let [Some(blocked), Some(ignored), Some(caught)] =
            ["SigBlk:", "SigIgn:", "SigCgt:"].map(field)
        else {
            return None;
        };
        let ttin = 1_u64 << (libc::SIGTTIN - 1);
        for mask in [blocked, ignored, caught] {
            assert_eq!(
                u64::from_str_radix(mask, 16).ok()? & ttin,
                0,
                "SIGTTIN disposition in {status}"
            );
        }
        let wait = fs::read_to_string(path.join("wchan")).unwrap_or_default();
        evidence.push(format!(
            "TID {} {state}, wchan {}, SigBlk {blocked}, SigIgn {ignored}, SigCgt {caught}",
            path.file_name()?.to_string_lossy(),
            wait.trim()
        ));
    }
    (!evidence.is_empty()).then(|| evidence.join("; "))
}

fn check_terminal_reclaim(ctx: &crate::Ctx, supervisor: &Supervisor<'_>) {
    use std::os::fd::AsRawFd;

    let (tty, caller) = foreground_terminal().unwrap();
    let fd = tty.as_raw_fd();
    let tty_modes = || {
        let mut modes = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(unsafe { libc::tcgetattr(fd, modes.as_mut_ptr()) }, 0);
        unsafe { modes.assume_init() }
    };
    let original = tty_modes();
    let mut changed = original;
    changed.c_lflag ^= libc::ECHO;
    let unrelated = supervisor
        .spawn(shell("exec sleep 10", &[]), Placement::Probe, false)
        .unwrap();
    for next in [caller, unrelated.entry.pid] {
        let mut job = TerminalJob::start(ctx, None).unwrap().unwrap();
        job.enter().unwrap();
        terminal::without_sigttou(|| {
            assert_eq!(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &changed) }, 0);
            assert_eq!(unsafe { libc::tcsetpgrp(fd, next) }, 0);
            Ok(())
        })
        .unwrap();
        let _ = job.fence().unwrap();
        assert_eq!(unsafe { libc::tcgetpgrp(fd) }, next);
        if next != caller {
            assert_eq!(tty_modes().c_lflag, changed.c_lflag);
            terminal::without_sigttou(|| {
                assert_eq!(unsafe { libc::tcsetpgrp(fd, caller) }, 0);
                Ok(())
            })
            .unwrap();
        } else {
            assert_eq!(tty_modes().c_lflag, original.c_lflag);
        }
        drop(job);
        assert_eq!(tty_modes().c_lflag, original.c_lflag);
    }
}
