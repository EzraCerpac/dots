mod support;

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::libc;
use serde_json::Value;
use support::World;

const FAILURE_TAIL: &str = "fixture-alpha-last-diagnostic";
const BETA_OUTPUT: &str = "fixture-beta-output";
const CHANGE: &str = "fixture-latest-change";

fn executable(path: &Path, script: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn fixture() -> World {
    let world = World::new();
    fs::write(
        world.root().join("config.toml"),
        "[vars]\ndots_profiles = 'laptop'\n",
    )
    .unwrap();
    fs::write(world.root().join("config.laptop.toml"), "").unwrap();
    fs::remove_file(world.root().join("dots/exceptions.tsv")).unwrap();
    fs::remove_dir_all(world.root().join("dots/hooks/update.d")).unwrap();
    executable(
        &world.root().join("bin/mise"),
        include_str!("supervisor_legacy/mise.sh"),
    );
    executable(
        &world.root().join("bin/hostname"),
        "#!/bin/sh\nprintf 'fixture-host\\n'\n",
    );
    executable(
        &world.root().join("bin/sudo"),
        "#!/bin/sh\n: > \"$DOTS_ROOT/unexpected-sudo\"\nexit 97\n",
    );
    world
}

fn dots(world: &World) -> Command {
    let mut command = world.dots();
    let bin = world.root().join("bin");
    let path = command
        .get_envs()
        .find_map(|(key, value)| (key == "PATH").then_some(value).flatten())
        .unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(path)))
            .unwrap();
    command
        .env("PATH", path)
        .env("DOTS_MISE_BIN", bin.join("mise"))
        .env("SETUP_PROFILE", "laptop")
        .env("MISE_SELF_UPDATE_AVAILABLE", "0")
        .env("CI", "1");
    command
}

fn hook(world: &World, name: &str, script: &str) {
    executable(&world.root().join("dots/hooks/update.d").join(name), script);
}

fn output(command: &mut Command, expected: i32) -> Output {
    let result = command.output().unwrap();
    assert_eq!(
        result.status.code(),
        Some(expected),
        "unexpected status for {:?} ({} stdout bytes, {} stderr bytes)",
        command.get_args(),
        result.stdout.len(),
        result.stderr.len(),
    );
    result
}

fn combined(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn once(text: &str, marker: &str) {
    assert_eq!(text.matches(marker).count(), 1, "expected one {marker:?}");
}

fn state(world: &World) -> PathBuf {
    world.home().join(".local/state/dots")
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            files.extend(self::files(&entry.path()));
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    files
}

fn reports(world: &World, command: &str) -> Vec<(PathBuf, Value)> {
    files(&state(world))
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let report: Value = serde_json::from_reader(File::open(&path).unwrap()).ok()?;
            let finished = report
                .get("finished_at_ms")
                .or_else(|| report.get("finished_at"));
            (report["command"] == command && finished.is_some()).then_some((path, report))
        })
        .collect()
}

fn assert_private(world: &World, path: &Path) {
    let state = state(world);
    for (index, entry) in path
        .ancestors()
        .take_while(|entry| entry.starts_with(&state))
        .enumerate()
    {
        let mode = fs::symlink_metadata(entry).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            if index == 0 { 0o600 } else { 0o700 },
            "public permissions on fixture report or log"
        );
    }
}

#[test]
#[ignore = "P-02"]
fn unchanged_redirected_test_preserves_binary_streams_and_private_metadata() {
    for (flag, code) in [(None, 0), (Some("--plain"), 23), (Some("--verbose"), 23)] {
        let world = fixture();
        executable(
            &world.root().join("tests/run"),
            "#!/bin/sh\ncat\nprintf 'fixture-stderr\\000\\377\\r' >&2\nexit \"$1\"\n",
        );
        let input = b"fixture-stdin\0\xff\r\n\x1b[31mstdout\x1b[0m";
        let source = world.root().join("input");
        fs::write(&source, input).unwrap();

        let output = output(
            dots(&world)
                .args(flag)
                .arg("test")
                .arg(code.to_string())
                .stdin(File::open(source).unwrap()),
            code,
        );
        assert!(
            output.stdout == input,
            "stdout differs from the binary fixture input"
        );
        assert!(
            output.stderr == b"fixture-stderr\0\xff\r",
            "stderr differs from the fixture diagnostic"
        );

        let saved = reports(&world, "test");
        assert_eq!(saved.len(), 1);
        let (path, report) = &saved[0];
        assert_private(&world, path);
        assert_eq!(
            report["status"]
                .as_i64()
                .or_else(|| report["exit_code"].as_i64()),
            Some(i64::from(code))
        );
        // The formats are private and differ. Check the recorded outcome in each.
        if let Some(stages) = report["stages"].as_array() {
            assert!(stages.iter().any(|stage| stage["outcome"] == if code == 0 { "completed" } else { "failed" }));
            assert!(
                stages
                    .iter()
                    .flat_map(|stage| stage["issues"].as_array().into_iter().flatten())
                    .all(|issue| issue["tail"].is_null() && issue["log"].is_null())
            );
        } else {
            let events = report["events"].as_array().unwrap();
            assert!(events.iter().any(|event| event["kind"] == "stage_end"
                && event["status"] == if code == 0 { "ok" } else { "failed" }));
            assert!(events.iter().all(|event| event.get("log_path").is_none()));
        }
        for file in files(&state(&world)) {
            let content = fs::read(file).unwrap();
            assert!(
                !content
                    .windows(b"fixture-stdin".len())
                    .any(|part| part == b"fixture-stdin")
            );
            assert!(
                !content
                    .windows(b"fixture-stderr".len())
                    .any(|part| part == b"fixture-stderr")
            );
        }
    }
}

#[test]
#[ignore = "P-04"]
fn unchanged_redirected_up_defaults_to_plain_and_verbose_shows_child_output() {
    for flag in [None, Some("--plain"), Some("--verbose")] {
        let world = fixture();
        hook(
            &world,
            "10-Text-mode",
            "#!/bin/sh\nprintf 'run\\n' >> \"$DOTS_ROOT/invocations\"\nprintf 'fixture-hook-output\\n'\nprintf 'fixture-hook-stderr\\n' >&2\n",
        );
        let result = output(dots(&world).args(flag).arg("up"), 0);
        let text = combined(&result);
        assert!(!text.contains("\x1b[?1049"));
        for expected in ["fixture-hook-output", "fixture-hook-stderr"] {
            assert_eq!(
                text.matches(expected).count(),
                usize::from(flag == Some("--verbose"))
            );
        }
        assert_eq!(
            fs::read(world.root().join("invocations")).unwrap(),
            b"run\n"
        );
        assert!(!world.root().join("unexpected-sudo").exists());
    }
}

#[test]
#[ignore = "P-04"]
fn new_plain_up_hook_reads_redirected_stdin() {
    let world = fixture();
    hook(
        &world,
        "10-Read-redirected-input",
        "#!/bin/sh\nIFS= read -r input || exit 91\nprintf '%s\\n' \"$input\" > \"$DOTS_ROOT/hook-input\"\n",
    );
    let input = world.root().join("answer");
    fs::write(&input, "fixture-answer\n").unwrap();
    output(
        dots(&world)
            .args(["up", "--plain"])
            .stdin(File::open(input).unwrap()),
        0,
    );
    assert_eq!(
        fs::read(world.root().join("hook-input")).unwrap(),
        b"fixture-answer\n"
    );
}

#[test]
#[ignore = "P-11"]
fn unchanged_up_keeps_stage_output_and_replays_the_failed_stages_tail() {
    let world = fixture();
    hook(
        &world,
        "10-Alpha",
        &format!(
            "#!/bin/sh\nprintf 'fixture-alpha-early\\n'\ni=0; while [ \"$i\" -lt 80 ]; do printf 'filler\\n'; i=$((i+1)); done\nprintf '{FAILURE_TAIL}\\n' >&2\nexit 7\n"
        ),
    );
    hook(
        &world,
        "20-Beta",
        &format!("#!/bin/sh\nprintf '{BETA_OUTPUT}\\n'\n"),
    );
    let live = output(dots(&world).args(["up", "--plain"]), 1);
    let mut alpha = None;
    let mut beta = None;
    for path in files(&state(&world))
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
    {
        let data = fs::read_to_string(&path).unwrap();
        if data.lines().any(|line| line == "fixture-alpha-early") && !data.contains(BETA_OUTPUT) {
            assert!(data.lines().any(|line| line == FAILURE_TAIL));
            assert_private(&world, &path);
            assert!(alpha.replace(path).is_none(), "duplicate Alpha stage log");
        } else if data.lines().any(|line| line == BETA_OUTPUT)
            && !data.contains("fixture-alpha-early")
            && !data.contains(FAILURE_TAIL)
        {
            assert_private(&world, &path);
            assert!(beta.replace(path).is_none(), "duplicate Beta stage log");
        }
    }
    let alpha = alpha.expect("missing Alpha stage log");
    assert!(beta.is_some(), "missing Beta stage log");
    let replay = output(dots(&world).args(["up", "--last"]), 1);
    for result in [&live, &replay] {
        let text = combined(result);
        once(&text, FAILURE_TAIL);
        assert!(text.contains("Alpha"));
        assert!(text.contains("dots up"), "missing rerun guidance");
        assert!(
            text.contains(alpha.to_str().unwrap()),
            "missing Alpha log link"
        );
        assert!(
            !text.contains("fixture-alpha-early"),
            "printed the full log instead of the tail"
        );
        assert!(
            !text.contains(BETA_OUTPUT),
            "the healthy stage's output leaked into the failure"
        );
    }
}

#[test]
#[ignore = "P-11"]
fn unchanged_plain_up_replays_latest_changes_and_attention_despite_exit_zero() {
    let world = fixture();
    hook(
        &world,
        "10-Change",
        "#!/bin/sh\nprintf 'fixture-earlier-change\\t1\\t2\\n' >> \"$DOTS_CHANGES\"\n",
    );
    let first = output(dots(&world).args(["up", "--plain"]), 0);
    once(&combined(&first), "fixture-earlier-change");
    hook(
        &world,
        "10-Change",
        &format!("#!/bin/sh\nprintf '{CHANGE}\\t11\\t22\\n' >> \"$DOTS_CHANGES\"\n"),
    );
    hook(
        &world,
        "20-Deferred",
        "#!/bin/sh\nprintf 'fixture-review-required\\n' >&2\nexit 3\n",
    );
    let live = output(dots(&world).args(["up", "--plain"]), 0);
    fs::write(world.root().join("no-more-mise"), "").unwrap();
    let replay = output(dots(&world).args(["up", "--last"]), 0);
    assert!(!world.root().join("replay-called-mise").exists());
    for result in [&live, &replay] {
        let text = combined(result);
        once(&text, CHANGE);
        once(&text, "fixture-review-required");
        assert!(text.contains("11") && text.contains("22"));
        assert!(text.contains("Deferred"));
        assert!(text.to_ascii_lowercase().contains("needs attention"));
        assert!(!text.contains("fixture-earlier-change"));
    }
    assert!(
        !combined(&replay).contains("dots up --last"),
        "replay advertised itself"
    );
}

#[test]
#[ignore = "P-11"]
fn unchanged_up_reports_one_config_diagnostic_when_state_is_unusable() {
    let world = fixture();
    let blocked = world.home().join("blocked-state");
    fs::write(&blocked, "fixture blocker").unwrap();
    let result = output(
        dots(&world)
            .args(["up", "--plain"])
            .env("FIXTURE_REJECT_CONFIG", "1")
            .env("XDG_STATE_HOME", blocked),
        1,
    );
    let stderr = String::from_utf8_lossy(&result.stderr).to_lowercase();
    assert!(
        stderr.contains("dots up") && stderr.contains("report"),
        "missing report warning naming the run"
    );
    let text = combined(&result);
    once(&text, "fixture-config-rejected");
    assert!(text.contains("Next:") && text.contains("rerun dots up"));
    assert!(!text.contains("Log:") && !text.contains("dots up --last"));
}

struct HeldUpdate {
    child: Child,
    release: PathBuf,
}

impl HeldUpdate {
    fn start(world: &World) -> Self {
        let child = dots(world)
            .args(["up", "--plain"])
            .env("FIXTURE_HOLD", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut update = Self {
            child,
            release: world.root().join("release-hook"),
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !world.root().join("held-hook-ready").exists() {
            assert!(
                update.child.try_wait().unwrap().is_none(),
                "update exited before the fixture hook"
            );
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the fixture hook"
            );
            thread::sleep(Duration::from_millis(10));
        }
        update
    }

    fn release(mut self) {
        fs::write(&self.release, "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "held update exited {status}");
                break;
            }
            assert!(Instant::now() < deadline, "released update did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for HeldUpdate {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // Wake the bounded fixture even if an assertion failed.
            let _ = fs::write(&self.release, "");
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn holding_hook(world: &World) {
    hook(
        world,
        "10-Hold",
        "#!/bin/sh\nif [ \"${FIXTURE_HOLD:-0}\" = 1 ]; then\n  printf 'fixture-held-stage\\n'\n  : > \"$DOTS_ROOT/held-hook-ready\"\n  i=0\n  until [ -f \"$DOTS_ROOT/release-hook\" ]; do\n    [ \"$i\" -lt 600 ] || exit 93\n    sleep 0.1\n    i=$((i+1))\n  done\nfi\n",
    );
}

#[test]
#[ignore = "P-11"]
fn unchanged_report_retention_prunes_old_completed_runs_and_keeps_active_output() {
    let world = fixture();
    holding_hook(&world);
    // Configuration rejection creates a real completed report without doing
    // unnecessary maintenance in the churn used to exercise pruning.
    let complete = || {
        output(
            dots(&world)
                .args(["up", "--plain"])
                .env("FIXTURE_REJECT_CONFIG", "1"),
            1,
        )
    };
    complete();
    let oldest = reports(&world, "up").pop().expect("missing first report").0;
    let active = HeldUpdate::start(&world);
    for _ in 0..10 {
        complete();
    }
    assert!(
        !oldest.exists(),
        "oldest completed report survived retention"
    );
    let recent = reports(&world, "up");
    assert_eq!(recent.len(), 10);
    for (path, _) in recent {
        assert_private(&world, &path);
    }
    active.release();
    let saved = reports(&world, "up");
    assert_eq!(saved.len(), 10);
    assert!(
        saved
            .iter()
            .any(|(_, report)| report["status"] == 0 || report["exit_code"] == 0)
    );
    assert!(
        files(&state(&world)).iter().any(|path| {
            path.extension().is_some_and(|ext| ext == "log")
                && fs::read_to_string(path)
                    .unwrap()
                    .lines()
                    .any(|line| line == "fixture-held-stage")
        }),
        "the active run lost its stage output during pruning"
    );
}

#[test]
#[ignore = "P-11"]
fn unchanged_redirected_up_ignores_window_resize() {
    let world = fixture();
    holding_hook(&world);
    let active = HeldUpdate::start(&world);
    // The ready fixture guarantees the signal reaches the live supervisor.
    assert_eq!(
        unsafe { libc::kill(active.child.id().try_into().unwrap(), libc::SIGWINCH) },
        0
    );
    active.release();
}
