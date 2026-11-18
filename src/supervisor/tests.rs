use super::*;
use std::fs;
use std::path::Path;

fn supervisor_at(path: &Path) -> Supervisor {
    let env = Env {
        vars: [("XDG_STATE_HOME".into(), path.as_os_str().into())].into(),
        cwd: path.to_owned(),
    };
    Supervisor::new(&env, Mode::Direct)
}

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
    let supervisor = supervisor_at(fixture.path());
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
            let mut stage = supervisor
                .spawn(command, Placement::Background, false)
                .unwrap();
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
                    entry.foreground
                        && matches!(observe(entry.pid, false), Ok(Observed::Exited { .. }))
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
    let supervisor = supervisor_at(fixture.path());
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
        vars: [
            ("DOTS_ROOT".into(), path.as_os_str().into()),
            ("XDG_STATE_HOME".into(), path.as_os_str().into()),
        ]
        .into(),
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
    for name in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "CODEX_HOME",
        "CLAUDE_CONFIG_DIR",
        "MISE_CONFIG_DIR",
        "MISE_CACHE_DIR",
        "MISE_DATA_DIR",
        "MISE_STATE_DIR",
    ] {
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
    let supervisor = supervisor_at(fixture.path());
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
fn captured_foreground_interrupt_escalates_against_ignoring_leader() {
    let fixture = tempfile::tempdir().unwrap();
    let supervisor = supervisor_at(fixture.path());
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
