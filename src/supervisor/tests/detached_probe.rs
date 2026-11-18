use super::*;

#[test]
fn detached_result_deadline_preserves_admission() {
    use crate::run::{Outcome, Run, Sink};

    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    let run = Run::begin(&ctx, Sink::Unreported);
    for command in [
        "exec sleep 10",
        "while :; do printf out; printf err >&2; sleep .02; done",
        "sleep 10 & exit 0",
    ] {
        let start = Instant::now();
        let error = run
            .probe_detached(shell(command, &[]), Duration::from_millis(300))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{command}: {error}");
        assert!(start.elapsed() < Duration::from_secs(4), "{command}");
        assert!(!run.cancelled());
    }

    let id = run.start("Later work");
    let output = run
        .probe_detached(
            shell("read line && exit 31; printf out; printf err >&2", &[]),
            Duration::from_secs(3),
        )
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"out");
    assert_eq!(output.stderr, b"err");
    run.finish(id, Outcome::Completed);
    assert_eq!(run.end(0), 0);
}

#[test]
fn detached_probe_rejects_custom_sigchld_before_spawn() {
    const FLAG: &str = "DOTS_SUPERVISOR_CUSTOM_SIGCHLD";
    if let Some(path) = std::env::var_os(FLAG) {
        extern "C" fn caught(_: libc::c_int) {}

        let path = Path::new(&path);
        let marker = path.join("spawned");
        let ctx = fixture_context(path, false);
        let supervisor = Supervisor::new(&ctx.env, Mode::Direct);
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = caught as *const () as libc::sighandler_t;
        assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
        assert_eq!(
            unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
            0
        );
        let error = supervisor
            .probe_detached(shell(": > \"$1\"", &[&marker]), Duration::from_secs(2))
            .unwrap_err();
        assert!(
            error.to_string().contains("requires waitable child"),
            "{error}"
        );
        assert!(!marker.exists());
        assert!(supervisor.state().children.is_empty());
        return;
    }

    let fixture = tempfile::tempdir().unwrap();
    let output = fixture_process(
        fixture.path(),
        FLAG,
        "supervisor::tests::detached_probe::detached_probe_rejects_custom_sigchld_before_spawn",
        b"",
    );
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn detached_probe_never_signals_a_group_without_its_waitable_leader() {
    let fixture = tempfile::tempdir().unwrap();
    let ctx = fixture_context(fixture.path(), false);
    let unrelated = Supervisor::new(&ctx.env, Mode::Direct);
    let sentinel = unrelated
        .spawn(shell("exec sleep 10", &[]), Placement::Detached, false)
        .unwrap();

    for route in ["poll", "cancel", "drop"] {
        let supervisor = Supervisor::new(&ctx.env, Mode::Direct);
        let mut probe = supervisor
            .spawn(shell("exit 0", &[]), Placement::DetachedProbe, false)
            .unwrap();
        assert!(probe.child.wait().unwrap().success());
        // Simulate a recycled group number with a live, test-owned sentinel.
        probe.entry.target = sentinel.entry.target;
        supervisor.state().children[0].target = sentinel.entry.target;
        match route {
            "poll" => assert_eq!(
                probe.poll(true, false).unwrap_err().raw_os_error(),
                Some(libc::ECHILD)
            ),
            "cancel" => supervisor.state().cancel(Some(libc::SIGINT), None),
            _ => {}
        }
        drop(probe);
        assert!(supervisor.state().children.is_empty(), "{route}");
        assert!(
            supervisor.state().warnings[0].contains("lost ownership"),
            "{route}"
        );
        assert!(
            matches!(observe(sentinel.entry.pid, false), Ok(Observed::Running)),
            "{route}"
        );
    }
}
