use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Barrier;

use super::*;

fn fixture_env(root: &Path) -> Env {
    Env {
        vars: [(OsString::from("XDG_STATE_HOME"), root.into())].into(),
        cwd: root.into(),
    }
}

fn update_report(finished_at_ms: u64, status: u8) -> Report {
    Report {
        command: Command::Up,
        finished_at_ms,
        status,
        cancelled: false,
        next: None,
        stages: Vec::new(),
    }
}

#[test]
fn saved_report_preserves_raw_log_path() {
    let fixture = tempfile::tempdir().unwrap();
    let env = fixture_env(fixture.path());
    let log = fixture.path().join(OsStr::from_bytes(b"capture-\xff.log"));
    #[cfg(target_os = "linux")]
    fs::write(&log, b"fixture stage output").unwrap();

    let mut issue = Issue::new("Stage failed.", "Retry.");
    issue.log = Some(log);
    let mut text_issue = Issue::new("Another stage failed.", "Retry.");
    text_issue.log = Some(PathBuf::from("/fixture/ordinary.log"));
    let mut report = update_report(100, 23);
    report.stages.push(Stage {
        id: StageId(0),
        label: "Fixture stage".into(),
        alongside: false,
        outcome: Some(Outcome::Failed),
        changes: Vec::new(),
        issues: vec![issue, text_issue],
    });

    save(&env, &report, None).expect("save a report containing a raw-byte log path");
    let entries = list(&env, Some(Command::Up)).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(load(&env, &entries[0].id).unwrap(), report);
    let json: serde_json::Value = read_json(&path(&env, &entries[0].id).unwrap()).unwrap();
    assert_eq!(
        json["stages"][0]["issues"][1]["log"],
        "/fixture/ordinary.log"
    );
}

#[test]
fn last_saved_update_survives_clock_rollback_and_retention() {
    let fixture = tempfile::tempdir().unwrap();
    let env = fixture_env(fixture.path());
    for minute in 0..10 {
        save(&env, &update_report(1000 + minute, minute as u8), None).unwrap();
    }
    save(&env, &update_report(900, 23), None).unwrap();

    let latest = list(&env, Some(Command::Up)).unwrap();
    let replay = load(&env, &latest[0].id).unwrap();
    prune(&env, Command::Up).unwrap();
    let retained = list(&env, Some(Command::Up)).unwrap();
    let statuses: Vec<_> = retained.iter().map(|entry| entry.status).collect();
    assert_eq!(
        (replay.status, statuses),
        (23, vec![23, 9, 8, 7, 6, 5, 4, 3, 2, 1]),
        "last saved report must be selected and retained after the wall clock moves back"
    );
    assert_eq!(replay.finished_at_ms, 900);

    save(&env, &update_report(900, 24), None).unwrap();
    let tied = list(&env, Some(Command::Up)).unwrap();
    assert_eq!((tied[0].status, tied[1].status), (24, 23));
}

#[test]
fn failed_and_unpublished_saves_leave_previous_reports_in_place() {
    let fixture = tempfile::tempdir().unwrap();
    let env = fixture_env(fixture.path());
    let directory = dir(&env).unwrap();
    for (name, time, status) in [
        ("1000-.pending-a.json", 1000, 1),
        ("1001-.pending-b.json", 1001, 2),
    ] {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(directory.join(name))
            .unwrap();
        serde_json::to_writer(file, &update_report(time, status)).unwrap();
    }
    save(&env, &update_report(900, 23), None).unwrap();

    // The file a stopped writer leaves before publication is ignored even if complete.
    let mut pending = Builder::new()
        .prefix(".pending-")
        .tempfile_in(&directory)
        .unwrap();
    serde_json::to_writer(&mut pending, &update_report(2000, 90)).unwrap();
    let _ = pending.keep().unwrap();

    fs::set_permissions(&directory, fs::Permissions::from_mode(0o750)).unwrap();
    let failed = save(&env, &update_report(3000, 99), None);
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(failed.unwrap_err().kind(), io::ErrorKind::PermissionDenied);

    let reports = list(&env, Some(Command::Up)).unwrap();
    assert_eq!(
        reports.iter().map(|entry| entry.status).collect::<Vec<_>>(),
        [23, 2, 1]
    );
    assert_eq!(load(&env, &reports[0].id).unwrap().finished_at_ms, 900);
}

#[test]
fn concurrent_saves_remain_ordered_and_retained() {
    let fixture = tempfile::tempdir().unwrap();
    let env = fixture_env(fixture.path());
    let ready = Barrier::new(4);
    std::thread::scope(|scope| {
        for first in [0, 3, 6, 9] {
            let (env, ready) = (&env, &ready);
            scope.spawn(move || {
                ready.wait();
                for status in first..first + 3 {
                    save(env, &update_report(900, status), None).unwrap();
                }
            });
        }
    });

    let reports = list(&env, Some(Command::Up)).unwrap();
    assert_eq!(reports.len(), 12);
    let statuses: Vec<_> = reports.iter().map(|entry| entry.status).collect();
    for first in [0, 3, 6, 9] {
        let writer: Vec<_> = statuses
            .iter()
            .copied()
            .filter(|status| (first..first + 3).contains(status))
            .collect();
        assert_eq!(writer, [first + 2, first + 1, first]);
    }

    prune(&env, Command::Up).unwrap();
    let retained: Vec<_> = list(&env, Some(Command::Up))
        .unwrap()
        .iter()
        .map(|entry| entry.status)
        .collect();
    assert_eq!(retained, statuses[..10]);
}
