mod support;

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::World;

const MINIMAL: &str = "[vars]\ndots_profiles = \"laptop server\"\n";

fn write(path: &Path, text: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn program(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|path| path.is_absolute() && path.is_file())
}

const MISE_DIRS: &[(&str, &str)] = &[
    ("MISE_DATA_DIR", "data"),
    ("MISE_CACHE_DIR", "cache"),
    ("MISE_STATE_DIR", "state"),
    ("MISE_TMP_DIR", "tmp"),
    ("MISE_DOWNLOADS_DIR", "downloads"),
    ("MISE_PLUGINS_DIR", "plugins"),
    ("MISE_INSTALLS_DIR", "installs"),
    ("MISE_INSTALL_STORE_DIR", "store"),
    ("MISE_SHIMS_DIR", "shims"),
    ("MISE_SYSTEM_CONFIG_DIR", "system-config"),
    ("MISE_SYSTEM_DATA_DIR", "system-data"),
    ("MISE_SYSTEM_PLUGINS_DIRS", "system-plugins"),
    ("MISE_SYSTEM_INSTALLS_DIR", "system-installs"),
    ("MISE_SHARED_INSTALL_DIRS", "shared-installs"),
];

fn isolate(world: &World, command: &mut Command) {
    let base = world.root().parent().unwrap();
    let private = world.home().join("mise-fixture");
    for dir in ["codex", "claude", "tmp", "bin"] {
        fs::create_dir_all(private.join(dir)).unwrap();
    }
    for (key, dir) in MISE_DIRS {
        let dir = private.join(dir);
        fs::create_dir_all(&dir).unwrap();
        command.env(key, dir);
    }
    command
        .env_remove("__MISE_DIFF")
        .env("P05_FIXTURE_BASE", base)
        .env("P05_MISE_DIRS", &private)
        .env("MISE_CONFIG_DIR", world.root())
        .env("MISE_CEILING_PATHS", base)
        .env("CODEX_HOME", private.join("codex"))
        .env("CLAUDE_CONFIG_DIR", private.join("claude"))
        .env("TMPDIR", private.join("tmp"));
    let path = env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(private.join("bin")).chain(
        env::split_paths(&path).filter(|path| path.is_absolute() && !path.ends_with("shims")),
    );
    command.env("PATH", env::join_paths(paths).unwrap());

    if let Some(real) = program("mise") {
        command.env("P05_REAL_MISE", real);
    }
    // Quiet selection validation reconstructs part of mise's environment. Check
    // the remaining values at the executable boundary before running real mise.
    let mut wrapper = String::from(
        r#"#!/bin/sh
set -eu
unset __MISE_DIFF
private_path() {
    case "$1" in
        "$P05_FIXTURE_BASE"/*) ;;
        *) echo "bootstrap fixture directory escaped isolation" >&2; exit 97 ;;
    esac
}
for path in "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$CODEX_HOME" "$CLAUDE_CONFIG_DIR" "$MISE_CONFIG_DIR" "$TMPDIR"; do
    private_path "$path"
done
"#,
    );
    for (key, dir) in MISE_DIRS {
        use std::fmt::Write as _;
        writeln!(
            wrapper,
            r#": "${{{key}:=$P05_MISE_DIRS/{dir}}}"
export {key}
private_path "${key}""#
        )
        .unwrap();
    }
    wrapper.push_str("exec \"$P05_REAL_MISE\" \"$@\"\n");
    let mise = private.join("bin/mise");
    write(&mise, &wrapper, 0o755);
    command
        .env("DOTS_MISE_BIN", &mise)
        .env("SETUP_MISE_BIN", mise);
}

fn dots(world: &World) -> Command {
    let mut command = world.dots();
    isolate(world, &mut command);
    command
}

fn age_keygen(world: &World, program: &Path) -> Command {
    let mut command = Command::new(program);
    let inherited = world.dots();
    command
        .env_clear()
        .envs(
            inherited
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .current_dir(world.root());
    isolate(world, &mut command);
    command
}

fn test_mise(world: &World) -> PathBuf {
    let fake = world.home().join("bin/mise");
    write(
        &fake,
        r#"#!/bin/sh
if [ "$1" = "--quiet" ]; then exec "$REAL_MISE" "$@"; fi
while [ "$1" = "-C" ] || [ "$1" = "-E" ]; do shift 2; done
case "$*" in
    "bootstrap dotfiles paths --json") printf '{"entries":[]}'; exit 0 ;;
    "bootstrap packages status --json"|ls*"--json") printf '{}'; exit 0 ;;
esac
case "$1 ${2-}" in
    "--quiet --no-config"|"config get"|"config set")
        exec "$REAL_MISE" "$@" ;;
    "exec --")
        if [ "${FIXTURE_REAL_EXEC:-0}" = 1 ]; then exec "$REAL_MISE" "$@"; fi
        shift 2
        case "$*" in
          *lib/source-repo.mjs*) exit 0 ;;
          uv\ *) exit 0 ;;
          *tasks/restore*) exit "${FIXTURE_RESTORE_STATUS:-7}" ;;
          *bootstrap/enroll-history*) exit 3 ;;
        esac
        exec "$@" ;;
    "bootstrap remote")
        printf '%s\n' "$@" ;;
    "bootstrap services")
        [ "$MISE_HISTORY_ENABLED" = 0 ] && [ -f "$MISE_GLOBAL_CONFIG_FILE" ] || exit 95
        "$REAL_MISE" bootstrap services status --json
        ;;
    "bootstrap "*)
        [ "${NATIVE_FIXTURE_STATUS:-0}" -eq 0 ] || exit "$NATIVE_FIXTURE_STATUS"
        if [ -n "${NATIVE_FIXTURE_CALLBACK:-}" ]; then
            exec "$NATIVE_FIXTURE_CALLBACK" bootstrap complete
        fi
        printf '%s\n' "$@" ;;
    "dot status") printf '{"history":{"watcher":"stopped"}}';;
    "which "*) exit 1 ;;
esac
exit 0
"#,
        0o755,
    );
    fake
}

fn fake(world: &World) -> Command {
    let mut command = dots(world);
    let base_mise = world.home().join("mise-fixture/bin/mise");
    let fake = test_mise(world);
    command
        .env("DOTS_MISE_BIN", &fake)
        .env("SETUP_MISE_BIN", fake)
        .env("REAL_MISE", base_mise)
        .env("SETUP_PROFILE", "laptop")
        .env("SETUP_MACHINE_ID", "fixture-host")
        .env("MISE_CONFIG_DIR", world.root())
        .env("MISE_TRUSTED_CONFIG_PATHS", world.root());
    command
}

fn legacy(world: &World) -> bool {
    dots(world)
        .args(["internal", "home"])
        .output()
        .unwrap()
        .status
        .success()
}

fn complete(world: &World) -> Command {
    let mut command = fake(world);
    command.args(if legacy(world) {
        ["internal", "complete"]
    } else {
        ["bootstrap", "complete"]
    });
    command
}

fn minimal(world: &World) {
    write(&world.root().join("config.toml"), MINIMAL, 0o644);
    fs::remove_dir_all(world.root().join("dots")).unwrap();
}

fn hook(world: &World, name: &str, body: &str, mode: u32) {
    let script = format!("#!/bin/sh\nset -eu\n{body}\n");
    write(
        &world.root().join("dots/hooks/bootstrap.d").join(name),
        &script,
        mode,
    );
}

fn run(command: &mut Command) -> Output {
    command.output().unwrap()
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).unwrap()
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn unchanged_configuration_command_and_help_use_the_fixture_profile() {
    let world = World::new();
    let help = run(dots(&world).arg("help"));
    assert_success(&help);
    assert!(stdout(&help).contains("hello [ARGS...]"), "{help:?}");
    let output = run(dots(&world)
        .args(["hello", "there"])
        .env("SETUP_PROFILE", "server")
        .env("DOTS_PROFILE", "server"));
    assert_success(&output);
    assert_eq!(stdout(&output), "hello there from server\n");
}

#[test]
#[ignore = "P-02"]
fn unchanged_fixture_bootstrap_hook_runs_for_each_profile() {
    let world = World::new();
    for profile in ["laptop", "server"] {
        let output = run(complete(&world).env("SETUP_PROFILE", profile));
        assert_success(&output);
        assert!(
            stdout(&output).contains(&format!("Login shell for {profile}")),
            "{output:?}"
        );
    }
}

#[test]
#[ignore = "P-04"]
fn unchanged_fixture_update_runs_profile_recipe_and_hooks() {
    let world = World::new();
    let legacy = legacy(&world);
    for profile in ["laptop", "server"] {
        let mut command = fake(&world);
        if legacy {
            command
                .args(["--backend", "up"])
                .env("DOTS_SOURCE_SYNC_DONE", "1");
        } else {
            command.args(["--plain", "up"]);
        }
        let output = run(command.env("SETUP_PROFILE", profile));
        assert_success(&output);
        let text = stdout(&output);
        assert!(
            text.contains(&format!("Hello from {profile}")),
            "{output:?}"
        );
        assert_eq!(
            text.contains("example-app recipe --install-only"),
            profile == "laptop",
            "{output:?}"
        );
        assert_eq!(
            text.contains("Skipped: nothing to back up in a fixture"),
            profile == "server",
            "{output:?}"
        );
    }
}

#[test]
#[ignore = "P-02"]
fn unchanged_bootstrap_hook_order_outcomes_and_readiness() {
    let world = World::new();
    let facts = world.home().join("facts");
    hook(&world, "40-after-failure", "touch \"$HOME/after\"", 0o755);
    hook(&world, "30-broken", "exit 7", 0o755);
    hook(&world, "20-later", "exit 3", 0o755);
    hook(&world, "25-not-here", "exit 4", 0o755);
    hook(
        &world,
        "10-Facts",
        r#"printf '%s\n' "$DOTS_ROOT" "$DOTS_PROFILE" "$DOTS_PRIVATE_READY" "$DOTS_RESTORED" "$PWD" > "$HOME/facts""#,
        0o755,
    );
    hook(&world, "50-skipped-mode", "exit 0", 0o644);
    hook(&world, "35-Laptop-only@laptop", "exit 0", 0o755);
    hook(
        &world,
        "36-Server-only@server",
        "touch \"$HOME/server\"",
        0o755,
    );
    let output = run(&mut complete(&world));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let text = stdout(&output);
    let outcomes = [
        "completed: Facts",
        "deferred: later",
        "skipped: not here",
        "failed: broken",
        "completed: Laptop only",
        "completed: after failure",
    ];
    let mut previous = text
        .find("example-app recipe --install-only")
        .expect("recipe must precede hooks");
    for outcome in outcomes {
        let next = text
            .find(outcome)
            .unwrap_or_else(|| panic!("missing {outcome}: {output:?}"));
        assert!(previous < next, "{output:?}");
        previous = next;
    }
    assert!(!text.contains("skipped mode"));
    assert!(!world.home().join("server").exists());
    assert!(world.home().join("after").exists());
    let root = world.root().display();
    assert_eq!(
        fs::read_to_string(facts).unwrap(),
        format!("{root}\nlaptop\n1\n0\n{root}\n")
    );
}

#[test]
#[ignore = "P-02"]
fn unchanged_unknown_hook_profile_stops_listing_before_any_hook() {
    let world = World::new();
    minimal(&world);
    hook(&world, "10-fine", "touch \"$HOME/ran\"", 0o755);
    hook(&world, "20-typo@labtop", "exit 0", 0o755);
    let output = run(&mut complete(&world));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stdout(&output).contains("failed: Bootstrap hooks"),
        "{output:?}"
    );
    assert!(stdout(&output).contains("labtop"), "{output:?}");
    assert!(!world.home().join("ran").exists());
}

#[test]
#[ignore = "P-02"]
fn unchanged_only_the_selected_profile_implicitly_requests_its_bundle() {
    let world = World::new();
    minimal(&world);
    write(
        &world.root().join("encrypted/bootstrap.laptop.json.age"),
        "fixture ciphertext",
        0o600,
    );
    for (profile, status) in [("laptop", "deferred"), ("server", "completed")] {
        let output = run(complete(&world).env("SETUP_PROFILE", profile));
        assert_success(&output);
        assert!(
            stdout(&output).contains(&format!("{status}: Private inputs")),
            "{output:?}"
        );
    }
}

#[test]
#[ignore = "P-02"]
fn unchanged_failed_restore_leaves_hook_readiness_false() {
    let world = World::new();
    minimal(&world);
    write(
        &world.root().join("config.toml"),
        &format!("{MINIMAL}dots_state_repo = \"file:///missing-fixture-history\"\n"),
        0o644,
    );
    write(&world.home().join(".config/age/keys.txt"), "fixture", 0o600);
    hook(
        &world,
        "10-Recovery",
        "printf '%s\n' \"$DOTS_RESTORED\" \"$DOTS_PRIVATE_READY\" > \"$HOME/readiness\"",
        0o755,
    );
    let output = run(complete(&world)
        .env("SETUP_RESTORE_MACHINE_ID", "fixture-host")
        .env("DOTS_BOOTSTRAP_RESTORE", "fixture-host"));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stdout(&output).contains("failed: Restore private settings"),
        "{output:?}"
    );
    assert_eq!(fs::read(world.home().join("readiness")).unwrap(), b"0\n0\n");
}

#[test]
fn new_complete_rejects_invalid_profile_configuration_with_usage_status() {
    let world = World::new();
    minimal(&world);
    let output = run(complete(&world).env("SETUP_PROFILE", "desktop"));
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("desktop"));
    write(
        &world.root().join("config.toml"),
        "# missing profiles\n",
        0o644,
    );
    let output = run(&mut complete(&world));
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("dots_profiles"));
}

#[test]
#[ignore = "P-02"]
fn new_minimal_completion_has_no_service_stages_or_standalone_report() {
    let world = World::new();
    minimal(&world);
    let output = run(&mut complete(&world));
    assert_success(&output);
    assert!(stdout(&output).contains("Private inputs"));
    assert!(stdout(&output).contains("Source repository"));
    for omitted in [
        "Atuin",
        "Tailscale",
        "Private configuration",
        "Encrypted local history",
    ] {
        assert!(!stdout(&output).contains(omitted), "{output:?}");
    }
    assert!(!world.home().join(".local/state/dots/reports").exists());
}

#[test]
fn new_persist_selected_keeps_the_existing_selection_and_uses_native_mise_env_for_first_run() {
    let world = World::new();
    minimal(&world);
    let output = run(fake(&world)
        .env_remove("SETUP_PROFILE")
        .env("MISE_ENV", "server,extra")
        .args(["bootstrap", "persist-selected"]));
    assert_success(&output);
    let second = run(fake(&world)
        .env("SETUP_MACHINE_ID", "new-id")
        .args(["bootstrap", "persist-selected"]));
    assert_success(&second);
    let local = world.root().join("config.local.toml");
    let selected = fs::read_to_string(&local).unwrap();
    assert!(
        selected.contains("SETUP_PROFILE = \"server\"")
            && selected.contains("SETUP_MACHINE_ID = \"fixture-host\""),
        "{selected}"
    );
    assert_eq!(
        fs::metadata(local).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn new_native_failure_says_complete_did_not_run() {
    let world = World::new();
    let output = run(fake(&world)
        .env("NATIVE_FIXTURE_STATUS", "7")
        .arg("bootstrap"));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("failed before dots bootstrap complete ran")
    );
}

#[test]
fn new_remote_uses_literal_url_and_rejects_a_git_rewrite() {
    let world = World::new();
    minimal(&world);
    let bin = world.home().join("bin");
    write(&bin.join("ssh"), "#!/bin/sh\nexit 92\n", 0o755);
    let mut paths = vec![bin];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let path = env::join_paths(paths).unwrap();
    write(
        &world.root().join("config.toml"),
        &format!("{MINIMAL}dots_source_repo = \"https://example.invalid/dotfiles\"\n"),
        0o644,
    );
    let output = run(fake(&world).env("PATH", &path).args([
        "remote",
        "fixture.invalid",
        "--profile",
        "server",
        "--dry-run",
        "--port",
        "8022",
    ]));
    assert_success(&output);
    assert!(
        stdout(&output).contains("--adopt\nhttps://example.invalid/dotfiles\n"),
        "{output:?}"
    );
    assert!(
        stdout(&output).contains("--remote-env\nserver\n"),
        "{output:?}"
    );
    assert!(stdout(&output).contains("--port\n8022\n"), "{output:?}");
    let rewrite = run(world.git().args([
        "config",
        "--global",
        "url.https://elsewhere.invalid/.pushInsteadOf",
        "https://example.invalid/",
    ]));
    assert_success(&rewrite);
    let rejected = run(fake(&world).env("PATH", &path).args([
        "remote",
        "fixture.invalid",
        "--profile",
        "server",
        "--dry-run",
    ]));
    assert_eq!(rejected.status.code(), Some(2), "{rejected:?}");
    assert!(rejected.stdout.is_empty(), "{rejected:?}");
}

#[test]
fn new_restore_stage_is_attempted_even_when_a_requested_bundle_is_missing() {
    let world = World::new();
    minimal(&world);
    write(
        &world.root().join("config.toml"),
        &format!("{MINIMAL}dots_state_repo = \"file:///missing-fixture-history\"\n"),
        0o644,
    );
    let identity = world.home().join("recovery-key");
    write(&identity, "fixture", 0o600);
    let output = run(complete(&world)
        .env("DOTS_BOOTSTRAP_RESTORE", "fixture-host")
        .env("DOTS_BOOTSTRAP_IDENTITY", &identity));
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        stdout(&output).contains("deferred: Private inputs"),
        "{output:?}"
    );
    assert!(
        stdout(&output).contains("failed: Restore private settings"),
        "{output:?}"
    );
    let local = fs::read_to_string(world.root().join("config.local.toml")).unwrap();
    assert!(local.contains("branch = \"host-fixture-host\""), "{local}");
    assert!(
        local.contains(&format!("SETUP_AGE_IDENTITY = \"{}\"", identity.display())),
        "{local}"
    );
}

#[test]
fn new_rootless_bundle_rejects_invalid_secret_without_printing_it() {
    let world = World::new();
    fs::remove_file(world.root().join("config.toml")).unwrap();
    let input = world.home().join("private.json");
    let output = world.home().join("bundle.age");
    write(
        &input,
        r#"{"version":1,"secrets":{"fixture-sensitive-marker":[1]}}"#,
        0o600,
    );
    let result = run(dots(&world)
        .args(["bundle", "create", "--input"])
        .arg(input)
        .arg("--output")
        .arg(&output)
        .args(["--recipient", "age1fixture"]));
    assert_eq!(result.status.code(), Some(1), "{result:?}");
    assert!(
        !format!(
            "{}{}",
            stdout(&result),
            String::from_utf8_lossy(&result.stderr)
        )
        .contains("fixture-sensitive-marker")
    );
    assert!(!output.exists());
}

#[test]
fn unchanged_bundle_round_trip_is_encrypted_and_exclusive() {
    let Some(keygen) = program("age-keygen") else {
        return;
    };
    let world = World::new();
    let identity = world.home().join("age-keys");
    let generated = run(age_keygen(&world, &keygen).arg("-o").arg(&identity));
    assert_success(&generated);
    let public = run(age_keygen(&world, &keygen).arg("-y").arg(&identity));
    assert_success(&public);
    let recipient = stdout(&public).trim();
    let input = world.home().join("private.json");
    let bundle = world.home().join("bundle.age");
    write(
        &input,
        r#"{"version":1,"secrets":{"github_token":"fixture-value-only\nsecond line"}}"#,
        0o600,
    );
    let create = || {
        run(fake(&world)
            .args(["bundle", "create", "--input"])
            .arg(&input)
            .arg("--output")
            .arg(&bundle)
            .args(["--recipient", recipient]))
    };
    let created = create();
    assert_success(&created);
    let encrypted = fs::read(&bundle).unwrap();
    assert!(encrypted.starts_with(b"age-encryption.org/v1\n"));
    assert!(
        !encrypted
            .windows(b"fixture-value-only".len())
            .any(|part| part == b"fixture-value-only")
    );
    assert_eq!(
        fs::metadata(&bundle).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!create().status.success());
    assert_eq!(fs::read(&bundle).unwrap(), encrypted);
    let checked = run(fake(&world)
        .args(["bundle", "check", "--bundle"])
        .arg(&bundle)
        .arg("--identity")
        .arg(&identity));
    assert_success(&checked);
}

#[test]
#[ignore = "P-02"]
fn new_hook_bundle_descriptors_and_folded_outcomes_are_private() {
    use std::io::Write as _;
    use std::process::Stdio;

    let world = World::new();
    minimal(&world);
    let bin = world.home().join("bin");
    let mut paths = vec![bin.clone()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let age = bin.join("age");
    write(
        &age,
        r#"#!/bin/sh
[ "$1" = --decrypt ] && [ -f "$3" ] && [ -f "$5" ] || exit 91
printf '%s' '{"version":1,"secrets":{"github_token":"fixture-token\nsecond line"}}'
"#,
        0o755,
    );
    let hook_body = r#"
        test "$DOTS_PRIVATE_READY" = 1
        test "$DOTS_RESTORED" = 0
        test -z "${GH_TOKEN+x}"
        test -z "${DOTS_STAGES+x}"
        body=$(cat "/dev/fd/$DOTS_BUNDLE_FD")
        test "$body" = '{"github_token":"fixture-token\nsecond line"}'
    "#;
    hook(
        &world,
        "10-First",
        &format!("{hook_body}\nIFS= read -r answer; test \"$answer\" = prompt-fixture"),
        0o755,
    );
    hook(&world, "20-Second", hook_body, 0o755);
    write(&world.home().join("recovery-key"), "fixture", 0o600);
    write(
        &world.home().join("bootstrap.age"),
        "fixture ciphertext",
        0o600,
    );
    let mut child = fake(&world)
        .env("PATH", env::join_paths(paths).unwrap())
        .env("NATIVE_FIXTURE_CALLBACK", world.dots().get_program())
        .env("FIXTURE_REAL_EXEC", "1")
        .current_dir(world.home())
        .args([
            "bootstrap",
            "--bundle",
            "bootstrap.age",
            "--identity",
            "recovery-key",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"prompt-fixture\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_success(&output);
    assert!(stdout(&output).contains("completed: First"), "{output:?}");
    assert!(stdout(&output).contains("completed: Second"), "{output:?}");
    let report_dir = world.home().join(".local/state/dots/reports");
    let report = fs::read_dir(report_dir)
        .unwrap()
        .find_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension()? == "json").then(|| fs::read_to_string(path).unwrap())
        })
        .expect("parent report");
    assert!(report.contains("Native mise bootstrap"));
    assert!(report.contains("First") && report.contains("Second"));
    for text in [
        &report,
        stdout(&output),
        &String::from_utf8_lossy(&output.stderr),
    ] {
        assert!(!text.contains("fixture-token"), "{text}");
    }
}

#[test]
fn new_pre_dotfiles_persists_selection_through_mise_x_before_tools() {
    let world = World::new();
    minimal(&world);
    let config = format!(
        r#"{MINIMAL}
[bootstrap.hooks]
pre-dotfiles = 'test "$MISE_HISTORY_ENABLED" = 0 && test ! -e "$HOME/applied" && SETUP_PROFILE=laptop "$DOTS_MISE_BIN" x -- dots bootstrap persist-selected'
[dotfiles]
"~/applied" = {{ source = "input", mode = "copy" }}
"~/tracked" = {{ mode = "track" }}
"#
    );
    write(&world.root().join("config.toml"), &config, 0o644);
    write(&world.root().join("input"), "fixture dotfile\n", 0o644);
    write(
        &world.home().join("tracked"),
        "synthetic unencrypted history input\n",
        0o600,
    );
    let output = run(dots(&world)
        .env("MISE_TRUSTED_CONFIG_PATHS", world.root())
        .env(
            "MISE_SYSTEM_CONFIG_FILE",
            world.home().join("empty-system.toml"),
        )
        .env("SETUP_MACHINE_ID", "fixture-host")
        .args(["bootstrap", "--yes"]));
    assert_success(&output);
    assert_eq!(
        fs::read_to_string(world.home().join("applied")).unwrap(),
        "fixture dotfile\n"
    );
    assert!(
        !world
            .home()
            .join("mise-fixture/state/history/repo.git")
            .exists()
    );
    let selection = fs::read_to_string(world.root().join("miserc.toml")).unwrap();
    assert!(
        selection.contains("laptop") && selection.contains("host-fixture-host"),
        "{selection}"
    );
}

#[test]
fn new_restore_declines_to_overwrite_recorded_recovery_or_origin() {
    let world = World::new();
    minimal(&world);
    write(
        &world.root().join("config.toml"),
        &format!("{MINIMAL}dots_state_repo = \"file:///expected-fixture-history\"\n"),
        0o644,
    );
    let identity = world.home().join("recovery-key");
    write(&identity, "fixture", 0o600);
    let local = world.root().join("config.local.toml");
    for (recorded, expected) in [
        (
            "SETUP_AGE_IDENTITY = \"~/other-key\"",
            "recorded recovery identity differs",
        ),
        (
            "SETUP_HISTORY_ORIGIN = \"file:///other-fixture-history\"",
            "history origin does not match",
        ),
    ] {
        let original = format!("[env]\n{recorded}\n");
        write(&local, &original, 0o600);
        let output = run(complete(&world)
            .env("DOTS_BOOTSTRAP_RESTORE", "fixture-host")
            .env("DOTS_BOOTSTRAP_IDENTITY", &identity));
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(stdout(&output).contains(expected), "{output:?}");
        assert_eq!(fs::read_to_string(&local).unwrap(), original);
    }
}

#[test]
fn new_source_stage_switches_only_the_clean_clone_to_dev() {
    let world = World::new();
    minimal(&world);
    assert_success(&run(world.git().args(["init", "--initial-branch=main"])));
    assert_success(&run(world.git().args(["add", "--all"])));
    assert_success(&run(world.git().args(["commit", "-m", "Seed fixture"])));
    let bare = world.bare_remote("origin");
    let origin = format!("file://{}", bare.display());
    assert_success(&run(world.git().args(["remote", "add", "origin", &origin])));
    assert_success(&run(world.git().args([
        "update-ref",
        "refs/remotes/origin/main",
        "HEAD",
    ])));
    assert_success(&run(world.git().args([
        "update-ref",
        "refs/remotes/origin/dev",
        "HEAD",
    ])));
    let config = world.root().join("config.toml");
    let dirty = format!("{MINIMAL}# unsaved fixture edit\n");
    write(&config, &dirty, 0o644);
    let left = run(&mut complete(&world));
    assert!(
        stdout(&left).contains("skipped: Source repository"),
        "{left:?}"
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), dirty);
    assert_eq!(
        stdout(&run(world.git().args(["branch", "--show-current"]))),
        "main\n"
    );

    write(&config, MINIMAL, 0o644);
    let moved = run(&mut complete(&world));
    assert!(
        stdout(&moved).contains("completed: Source repository"),
        "{moved:?}"
    );
    assert_eq!(
        stdout(&run(world.git().args(["branch", "--show-current"]))),
        "dev\n"
    );
}

#[test]
fn new_history_origin_alone_lists_enrollment_but_needs_a_state_repo_when_ready() {
    let world = World::new();
    minimal(&world);
    let identity = world.home().join("recovery.key");
    write(&identity, "synthetic fixture", 0o600);
    let outcome = run(complete(&world)
        .env("SETUP_HISTORY_ORIGIN", "file:///missing-fixture-history")
        .env("SETUP_AGE_IDENTITY", identity));
    assert_eq!(outcome.status.code(), Some(1), "{outcome:?}");
    assert!(
        stdout(&outcome).contains("failed: Encrypted local history"),
        "{outcome:?}"
    );
    assert!(stdout(&outcome).contains("dots_state_repo"), "{outcome:?}");
    assert!(!world.home().join(".config/age/keys.txt").exists());
}

#[test]
fn new_bootstrap_creates_only_a_missing_machine_key_after_recovery_is_ready() {
    let world = World::new();
    minimal(&world);
    write(
        &world.root().join("config.toml"),
        &format!("{MINIMAL}dots_state_repo = \"file:///fixture-history\"\n"),
        0o644,
    );
    let bin = world.home().join("bin");
    let mut paths = vec![bin.clone()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let path = env::join_paths(paths).unwrap();
    write(
        &bin.join("age-keygen"),
        r#"#!/bin/sh
case "${1-}" in
    -y) [ -f "$2" ] || exit 91; printf '%s\n' age1fixture-public | tr -d '-' ;;
    "") printf '%s\n' AGE-SECRET-KEY-FIXTURE-ONLY; printf 'generated\n' >> "$HOME/generation";;
    *) exit 91 ;;
esac
"#,
        0o755,
    );
    let recovery = world.home().join("recovery-key");
    let key = world.home().join(".config/age/keys.txt");
    let generation = world.home().join("generation");
    let complete = || {
        run(complete(&world)
            .env("PATH", &path)
            .env("SETUP_AGE_IDENTITY", &recovery))
    };
    let missing = complete();
    assert!(
        stdout(&missing).contains("deferred: Encrypted local history"),
        "{missing:?}"
    );
    assert!(!key.exists() && !generation.exists());

    write(&recovery, "fixture recovery identity", 0o600);
    complete();
    let generated = fs::read(&key).unwrap();
    assert_eq!(generated, b"AGE-SECRET-KEY-FIXTURE-ONLY\n");
    assert_eq!(
        fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(key.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    complete();
    assert_eq!(fs::read(key).unwrap(), generated);
    assert_eq!(fs::read(generation).unwrap(), b"generated\n");
    assert_eq!(
        fs::read_to_string(recovery).unwrap(),
        "fixture recovery identity"
    );
}

#[test]
fn unchanged_remote_delegates_native_adoption_and_connection_options() {
    let world = World::new();
    minimal(&world);
    write(
        &world.root().join("config.toml"),
        &format!("{MINIMAL}dots_source_repo = \"https://example.invalid/config\"\n"),
        0o644,
    );
    let fixture_bin = world.home().join("mise-fixture/bin");
    write(&fixture_bin.join("ssh"), "#!/bin/sh\nexit 92\n", 0o755);
    let output = run(fake(&world).args([
        "remote",
        "fixture.invalid",
        "--profile",
        "server",
        "--dry-run",
        "--yes",
        "--port",
        "8022",
        "--connect-timeout",
        "17",
    ]));
    assert_success(&output);
    let argv = stdout(&output);
    for expected in [
        "--host\nfixture.invalid\n",
        "--adopt\nhttps://example.invalid/config\n",
        "--remote-env\nserver\n",
        "--install-mise\n",
        "--dry-run\n",
        "--yes\n",
        "--port\n8022\n",
        "--connect-timeout\n17\n",
    ] {
        assert!(
            argv.contains(expected),
            "{expected:?} missing from {argv:?}"
        );
    }
}

#[test]
fn new_bootstrap_update_checks_the_effective_upstream_before_running_mise() {
    let world = World::new();
    minimal(&world);
    let expected = "https://expected.invalid/config";
    write(
        &world.root().join("config.toml"),
        &format!("{MINIMAL}dots_source_repo = \"{expected}\"\n"),
        0o644,
    );
    let git = |args: &[&str]| {
        let mut command = world.git();
        isolate(&world, &mut command);
        assert_success(&run(command.args(args)));
    };
    git(&["init", "--initial-branch=main"]);
    git(&["add", "--all"]);
    git(&["commit", "-m", "Seed fixture"]);
    git(&["remote", "add", "origin", expected]);
    git(&["remote", "add", "alternate", expected]);
    git(&["update-ref", "refs/remotes/alternate/main", "HEAD"]);
    git(&["branch", "--set-upstream-to=alternate/main"]);
    git(&[
        "config",
        "--add",
        "remote.alternate.pushurl",
        "https://other.invalid/config",
    ]);
    let output = run(fake(&world).args(["bootstrap", "--update"]));
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("outside the expected bootstrap repository")
    );

    git(&["config", "--unset-all", "remote.alternate.pushurl"]);
    let output = run(fake(&world).args(["bootstrap", "--update"]));
    assert_success(&output);
    assert!(stdout(&output).contains("--update"), "{output:?}");
}
