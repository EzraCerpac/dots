use super::*;
use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt as _;

thread_local! {
    static NATIVE_STARTUP_CHECK: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub(in crate::supervisor) fn wait_for_native_startup_check() {
    if let Some(marker) = NATIVE_STARTUP_CHECK.take() {
        assert!(
            wait_until(|| marker.exists()),
            "native payload never reached its startup tty check"
        );
    }
}

fn pin_observer(ctx: &crate::Ctx, path: &Path, payload: &str) {
    let bin = tempfile::tempdir_in(path).unwrap();
    let program = bin.path().join("dots");
    fs::write(
        &program,
        format!("#!/bin/sh\nPATH=/usr/bin:/bin\n{payload}\n"),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    ctx.root.child_bin.set(Ok(bin)).unwrap();
}

#[test]
fn audit_foreground_is_available_on_native_startup() {
    const TEST: &str = "supervisor::tests::audit::audit_foreground_is_available_on_native_startup";
    const NATIVE: &str = "DOTS_AUDIT_FOREGROUND_PAYLOAD";
    if let Some(root) = std::env::var_os(NATIVE) {
        let foreground = unsafe { libc::tcgetpgrp(0) };
        let group = unsafe { libc::getpgrp() };
        fs::write(Path::new(&root).join("native-startup-check"), b"").unwrap();
        assert_eq!(
            foreground, group,
            "native startup lacked the foreground tty"
        );
        let mut line = String::new();
        io::stdin().read_line(&mut line).unwrap();
        assert_eq!(line, "tty answer\n");
        assert_eq!(unsafe { libc::tcgetpgrp(0) }, unsafe { libc::getpgrp() });
        return;
    }
    run_tty_probe_fixture(TEST, |path| {
        println!("probe prompt");
        let ctx = fixture_context(path, false);
        pin_observer(&ctx, path, "printf RR; exec dd bs=1 count=1 2>/dev/null");
        let supervisor = Supervisor::new(&ctx, Mode::Direct);
        let mut command = fixture_exec(path, NATIVE, TEST);
        command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        NATIVE_STARTUP_CHECK.set(Some(path.join("native-startup-check")));
        assert!(
            supervisor
                .spawn(command, Placement::Foreground, true)
                .unwrap()
                .wait()
                .unwrap()
                .success()
        );
    });
}

#[test]
fn audit_terminal_fence_recovers_and_bounds_the_reply() {
    run_tty_probe_fixture(
        "supervisor::tests::audit::audit_terminal_fence_recovers_and_bounds_the_reply",
        |path| {
            println!("probe prompt");
            for responds in [true, false] {
                let ctx = fixture_context(path, false);
                let payload = if responds {
                    "printf RI; exec dd bs=1 count=1 2>/dev/null"
                } else {
                    "printf RR; exec sleep 60"
                };
                pin_observer(&ctx, path, payload);
                let mut job = TerminalJob::start(&ctx, None).unwrap().unwrap();
                job.enter().unwrap();
                if responds {
                    assert_eq!(unsafe { libc::kill(job.observer_pid(), libc::SIGSTOP) }, 0);
                    assert!(wait_until(|| matches!(
                        observe(job.observer_pid(), true),
                        Ok(Observed::Stopped { .. })
                    )));
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                let mut noticed = false;
                loop {
                    noticed |= job.notices().unwrap();
                    if job.fence().unwrap() {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "foreground fence can strand completion; responds={responds}"
                    );
                    std::thread::sleep(TICK);
                }
                assert_eq!(noticed, responds);
                assert_eq!(job.take_fence_timeout(), !responds);
                assert!(foreground_terminal().is_some());
                let observer = job.observer_pid();
                drop(job);
                assert_eq!(
                    observe(observer, false).err().unwrap().raw_os_error(),
                    Some(libc::ECHILD)
                );
            }
        },
    );
}

#[test]
fn audit_ordinary_stage_requires_waitable_sigchld_before_spawn() {
    const TEST: &str =
        "supervisor::tests::audit::audit_ordinary_stage_requires_waitable_sigchld_before_spawn";
    const FLAG: &str = "DOTS_AUDIT_SIGCHLD";
    if let Some(path) = std::env::var_os(FLAG) {
        extern "C" fn does_not_reap(_: libc::c_int) {}

        let set = |handler, flags| {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = handler;
            action.sa_flags = flags;
            assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
            assert_eq!(
                unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
                0
            );
        };
        for (handler, flags) in [(libc::SIG_IGN, 0), (libc::SIG_DFL, libc::SA_NOCLDWAIT)] {
            set(handler, flags);
            assert!(require_waitable_children().is_err());
        }
        set(does_not_reap as *const () as libc::sighandler_t, 0);
        let path = Path::new(&path);
        let ctx = fixture_context(path, false);
        let supervisor = Supervisor::new(&ctx, Mode::Direct);
        // A nonexistent executable cannot start a vulnerable or waitless child.
        let error = supervisor
            .spawn(
                Command::new(path.join("absent-program")),
                Placement::Foreground,
                true,
            )
            .err()
            .unwrap();
        assert!(error.to_string().contains("waitable child"), "{error}");
        assert!(supervisor.state().children.is_empty());
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let output = bounded_fixture(fixture.path(), FLAG, TEST);
    assert!(output.status.success(), "{output:?}");
}
