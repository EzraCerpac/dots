use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use dots_cli::report::{self, Report};
use dots_cli::ui::transcript;
use serde_json::json;

fn example_report() -> Report {
    serde_json::from_value(json!({
        "command": "up",
        "finished_at_ms": 1,
        "status": 0,
        "stages": [
            {
                "id": 0,
                "label": "App Store",
                "alongside": true,
                "outcome": "deferred",
                "changes": [],
                "issues": [{
                    "diagnostic": "Authorization expired",
                    "next": "dots up --mas",
                    "tail": "Authorization is required\nNo apps were updated",
                    "log": "/state/dots/app-store.log"
                }]
            },
            {
                "id": 1,
                "label": "Printer setup",
                "alongside": false,
                "outcome": "completed",
                "changes": [
                    {"item": "driver", "before": "1.2", "after": "1.3"},
                    {"item": "cartridge", "before": "not installed", "after": "2.0"},
                    {"item": "old-driver", "before": "0.9", "after": "not present"},
                    {"item": "defaults", "before": "", "after": ""},
                    {"item": "fonts", "before": "5", "after": "5"}
                ],
                "issues": []
            },
            {
                "id": 2,
                "label": "dots manager",
                "alongside": false,
                "outcome": "completed",
                "changes": [{
                    "item": "dots",
                    "before": "1.0",
                    "after": "1.1, used from the next run"
                }],
                "issues": []
            }
        ]
    }))
    .unwrap()
}

#[derive(Default)]
struct Scrollback {
    bytes: Vec<u8>,
    flushed_at: usize,
}

impl Write for Scrollback {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushed_at = self.bytes.len();
        Ok(())
    }
}

fn example_transcript(saved: bool) -> Vec<u8> {
    let report = example_report();
    let [app_store, printer, manager] = report.stages.as_slice() else {
        panic!("expected three stages");
    };
    let mut out = Scrollback::default();
    transcript::write_stage_start(&mut out, app_store).unwrap();
    transcript::write_stage_start(&mut out, printer).unwrap();
    assert_eq!(out.flushed_at, out.bytes.len());
    assert!(out.bytes.ends_with(b"==> Printer setup\n"));

    // Model the caller's ordering around inherited child output without a TTY.
    out.write_all(b"Printer name? office-laser\n").unwrap();
    transcript::write_stage_end(&mut out, printer).unwrap();
    transcript::write_stage_end(&mut out, app_store).unwrap();
    transcript::write_stage_start(&mut out, manager).unwrap();
    transcript::write_stage_end(&mut out, manager).unwrap();
    transcript::write_report(&mut out, &report, saved).unwrap();
    out.bytes
}

// The approved design leaves line wrapping to the terminal. These fixtures
// use ASCII, a middle dot and an arrow, each occupying one column in the fixture.
fn terminal_rows(bytes: &[u8], columns: usize) -> String {
    let mut rows = String::new();
    let mut column = 0;
    for ch in std::str::from_utf8(bytes).unwrap().chars() {
        assert!(ch.is_ascii() || matches!(ch, '·' | '→'));
        if ch == '\n' {
            column = 0;
        } else {
            if column == columns {
                rows.push('\n');
                column = 0;
            }
            column += 1;
        }
        rows.push(ch);
    }
    rows
}

#[test]
fn new_wide_scrollback() {
    assert_eq!(
        terminal_rows(&example_transcript(true), 100),
        concat!(
            "==> App Store (alongside)\n",
            "==> Printer setup\n",
            "Printer name? office-laser\n",
            "Printer setup: completed\n",
            "App Store (alongside): deferred\n",
            "==> dots manager\n",
            "dots manager: completed\n",
            "dots up: Completed with issues (exit 0)\n",
            "    updated  driver  1.2 → 1.3\n",
            "    installed  cartridge  2.0\n",
            "    removed  old-driver  0.9\n",
            "    changed  defaults\n",
            "    changed  fonts  5 → 5\n",
            "dots 1.0 -> 1.1, used from the next run\n",
            "Needs attention\n",
            "  App Store (alongside): Authorization expired\n",
            "    Next: dots up --mas\n",
            "    | Authorization is required\n",
            "    | No apps were updated\n",
            "    Log: /state/dots/app-store.log\n",
            "Details and logs · dots up --last\n",
        )
    );
}

#[test]
fn new_narrow_scrollback() {
    assert_eq!(
        terminal_rows(&example_transcript(false), 32),
        concat!(
            "==> App Store (alongside)\n",
            "==> Printer setup\n",
            "Printer name? office-laser\n",
            "Printer setup: completed\n",
            "App Store (alongside): deferred\n",
            "==> dots manager\n",
            "dots manager: completed\n",
            "dots up: Completed with issues (\n",
            "exit 0)\n",
            "    updated  driver  1.2 → 1.3\n",
            "    installed  cartridge  2.0\n",
            "    removed  old-driver  0.9\n",
            "    changed  defaults\n",
            "    changed  fonts  5 → 5\n",
            "dots 1.0 -> 1.1, used from the n\n",
            "ext run\n",
            "Needs attention\n",
            "  App Store (alongside): Authori\n",
            "zation expired\n",
            "    Next: dots up --mas\n",
            "    | Authorization is required\n",
            "    | No apps were updated\n",
            "    Log: /state/dots/app-store.l\n",
            "og\n",
        )
    );
}

#[test]
fn new_native_failure_looks_the_same_on_replay() {
    let mut report = example_report();
    report.status = 1;
    report.next = Some("dots up".into());
    let mut native = report.stages.remove(1);
    native.outcome = Some(report::Outcome::Failed);
    native.changes.clear();
    let mut issue = report::Issue::new("Printer setup failed", "Check printer settings and rerun.");
    issue.terminal_exit = Some(23);
    // A stale saved value must never be rendered as captured native output.
    issue.tail = Some("STORED TAIL".into());
    issue.log = Some("/state/invalid.log".into());
    native.issues.push(issue);
    report.stages = vec![native];

    let mut live = Vec::new();
    transcript::write_report(&mut live, &report, true).unwrap();
    assert_eq!(
        std::str::from_utf8(&live).unwrap(),
        concat!(
            "dots up: Failed (exit 1)\n",
            "Needs attention\n",
            "  Printer setup: Printer setup failed\n",
            "    Exit status: 23\n",
            "    Next: Check printer settings and rerun.\n",
            "    Output was shown on the terminal\n",
            "Rerun: dots up\n",
            "Details and logs · dots up --last\n",
        )
    );
    let saved = serde_json::to_vec(&report).unwrap();
    let loaded = serde_json::from_slice(&saved).unwrap();
    let mut replay = Vec::new();
    transcript::write_report(&mut replay, &loaded, true).unwrap();
    assert_eq!(replay, live);
}

#[test]
fn new_live_and_saved_reports_keep_unexplained_stage_dispositions() {
    let mut report = example_report();
    report.stages.truncate(1);
    for (outcome, disposition, status, result) in [
        (
            report::Outcome::Deferred,
            "deferred",
            0,
            "Completed with issues",
        ),
        (
            report::Outcome::Failed,
            "failed",
            0,
            "Completed with issues",
        ),
        (report::Outcome::Failed, "failed", 23, "Failed"),
    ] {
        report.status = status;
        report.stages[0].outcome = Some(outcome);
        report.stages[0].issues.clear();

        let expected = format!(
            "dots up: {result} (exit {status})\nNeeds attention\n  App Store (alongside) [{disposition}]\nDetails and logs · dots up --last\n"
        );
        let mut live = Vec::new();
        transcript::write_stage_end(&mut live, &report.stages[0]).unwrap();
        transcript::write_report(&mut live, &report, true).unwrap();
        assert_eq!(
            live,
            format!("App Store (alongside): {disposition}\n{expected}").as_bytes(),
        );

        let stored = serde_json::to_vec(&report).unwrap();
        let loaded = serde_json::from_slice(&stored).unwrap();
        let mut replay = Vec::new();
        transcript::write_report(&mut replay, &loaded, true).unwrap();
        assert_eq!(replay, expected.as_bytes());

        report.stages[0].issues.push(report::Issue::new(
            "The App Store is unavailable.",
            "Try dots up --mas again later.",
        ));
        let mut explained = Vec::new();
        transcript::write_report(&mut explained, &report, true).unwrap();
        assert_eq!(
            explained,
            format!(
                "dots up: {result} (exit {status})\nNeeds attention\n  App Store (alongside): The App Store is unavailable.\n    Next: Try dots up --mas again later.\nDetails and logs · dots up --last\n"
            )
            .as_bytes(),
        );
    }
}

#[test]
fn new_report_cancellation_is_independent_of_exit_status() {
    let mut report = example_report();
    report.stages[0].outcome = Some(report::Outcome::Cancelled);
    let mut boundary = Vec::new();
    transcript::write_stage_end(&mut boundary, &report.stages[0]).unwrap();
    assert_eq!(boundary, b"App Store (alongside): cancelled\n");
    report.cancelled = true;
    for status in [0, 23] {
        report.status = status;
        let mut output = Vec::new();
        transcript::write_report(&mut output, &report, false).unwrap();
        assert!(output.starts_with(format!("dots up: Cancelled (exit {status})\n").as_bytes()));
    }
    report.cancelled = false;
    report.status = 130;
    let mut output = Vec::new();
    transcript::write_report(&mut output, &report, false).unwrap();
    assert!(output.starts_with(b"dots up: Failed (exit 130)\n"));
}

#[test]
fn new_terminal_controls_in_metadata_are_visible_text() {
    let mut report = example_report();
    let stage = &mut report.stages[0];
    stage.label = "Café\r\x1b[2J\u{202e}".into();
    stage.changes.push(report::Change {
        item: "driver\nLabel".into(),
        before: "1\t2".into(),
        after: "3\x07".into(),
    });
    let issue = &mut stage.issues[0];
    issue.diagnostic = "bad\u{009b}2J".into();
    issue.next = "try\u{2028}again".into();
    issue.tail = Some("one\rupdated\ntwo\x1b]0;name\x07".into());
    issue.log = Some("/state/weird\nlog".into());
    report.next = Some("dots up\u{2067}".into());

    let mut output = Vec::new();
    transcript::write_stage_start(&mut output, &report.stages[0]).unwrap();
    transcript::write_stage_end(&mut output, &report.stages[0]).unwrap();
    transcript::write_report(&mut output, &report, false).unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.chars().all(|ch| ch == '\n' || !ch.is_control()));
    for expected in [
        "==> Café\\r\\u{1b}[2J\\u{202e} (alongside)",
        "Café\\r\\u{1b}[2J\\u{202e} (alongside): deferred",
        "updated  driver\\nLabel  1\\t2 → 3\\u{7}",
        "bad\\u{9b}2J",
        "Next: try\\u{2028}again",
        "    | one\\rupdated\n    | two\\u{1b}]0;name\\u{7}",
        "Log: /state/weird\\nlog",
        "Rerun: dots up\\u{2067}",
    ] {
        assert!(
            output.contains(expected),
            "missing {expected:?} in {output:?}"
        );
    }
}

#[test]
fn new_report_keeps_the_full_unknown_hook_profile_diagnostic() {
    let mut report = example_report();
    let mut hooks = report.stages.remove(1);
    hooks.label = "Update hooks".into();
    hooks.outcome = Some(report::Outcome::Failed);
    hooks.changes.clear();
    hooks.issues.push(report::Issue::new(
        "20-typo@labtop names unknown profile labtop. Configured profiles are laptop and server.",
        "Rename the hook with a configured profile.",
    ));
    report.stages = vec![hooks];
    report.status = 1;

    let mut output = Vec::new();
    transcript::write_stage_end(&mut output, &report.stages[0]).unwrap();
    transcript::write_report(&mut output, &report, false).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        concat!(
            "Update hooks: failed\n",
            "dots up: Failed (exit 1)\n",
            "Needs attention\n",
            "  Update hooks: 20-typo@labtop names unknown profile labtop. Configured profiles are laptop and server.\n",
            "    Next: Rename the hook with a configured profile.\n",
        )
    );
}

#[test]
fn new_log_paths_distinguish_raw_bytes_from_escapes() {
    let mut report = example_report();
    for (bytes, expected) in [
        (&b"/state/line\nnext"[..], r"/state/line\nnext"),
        (&br"/state/line\nnext"[..], r"/state/line\\nnext"),
        (&b"/state/\xff"[..], r"/state/\xff"),
        (&br"/state/\xff"[..], r"/state/\\xff"),
        ("/state/�".as_bytes(), "/state/�"),
        (
            "/state/\u{2028}\u{2029}\u{202e}\x1b".as_bytes(),
            r"/state/\u{2028}\u{2029}\u{202e}\u{1b}",
        ),
    ] {
        report.stages[0].issues[0].log = Some(Path::new(OsStr::from_bytes(bytes)).to_owned());
        let mut output = Vec::new();
        transcript::write_report(&mut output, &report, false).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output
                .lines()
                .find_map(|line| line.strip_prefix("    Log: ")),
            Some(expected),
        );
    }
}
