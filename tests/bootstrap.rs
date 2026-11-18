mod support;

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{FixtureCommand, World};

const MINIMAL: &str = "[vars]\ndots_profiles = \"laptop server\"\n";

fn write(path: &Path, text: &str, mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn program(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH").unwrap_or_default())
        .filter(|directory| !directory.ends_with("shims"))
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
    ("MISE_SYSTEM_SHIMS_DIR", "system-shims"),
    ("MISE_SHARED_INSTALL_DIRS", "shared-installs"),
];

fn isolate(world: &World, command: &mut FixtureCommand<'_>) {
    let base = world.base();
    let private = world.home().join("mise-fixture");
    for dir in ["codex", "claude", "tmp", "bin"] {
        fs::create_dir_all(private.join(dir)).unwrap();
    }
    for (key, dir) in MISE_DIRS {
        let dir = private.join(dir);
        fs::create_dir_all(&dir).unwrap();
        command.env(key, dir);
    }
    let guard_file = private.join("guard.sh");
    let guarded_exec = private.join("guarded-exec");
    command
        .env_remove("__MISE_DIFF")
        .env("P05_FIXTURE_BASE", base)
        .env("P05_MISE_DIRS", &private)
        .env("P05_FIXTURE_GUARD", &guard_file)
        .env("P05_GUARDED_EXEC", &guarded_exec)
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
    // FixtureCommand checks the initial launch. Recheck private paths where a
    // nested mise invocation or callback is about to execute.
    let mut guard = String::from(
        r#"unset __MISE_DIFF
outside_fixture() {
    echo "bootstrap fixture path escaped isolation" >&2
    exit 97
}
private_path() {
    case "$1" in */../*|*/..) outside_fixture ;; esac
    case "$1" in
        "$P05_FIXTURE_BASE"|"$P05_FIXTURE_BASE"/*) ;;
        *) outside_fixture ;;
    esac
    probe=$1
    while [ ! -d "$probe" ]; do
        [ ! -L "$probe" ] || outside_fixture
        probe=${probe%/*}
    done
    physical=$(CDPATH='' cd -P "$probe" && pwd -P) || outside_fixture
    case "$physical" in
        "$P05_FIXTURE_BASE"|"$P05_FIXTURE_BASE"/*) ;;
        *) outside_fixture ;;
    esac
}
for path in "$(pwd -P)" "$HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" "$XDG_CACHE_HOME" "$CODEX_HOME" "$CLAUDE_CONFIG_DIR" "$MISE_CONFIG_DIR" "$TMPDIR"; do
    private_path "$path"
done
for path in "${MISE_CONFIG_FILE:-}" "${MISE_GLOBAL_CONFIG_FILE:-}" "${MISE_SYSTEM_CONFIG_FILE:-}"; do
    if [ -n "$path" ]; then private_path "$path"; fi
done
"#,
    );
    for (key, dir) in MISE_DIRS {
        use std::fmt::Write as _;
        writeln!(
            guard,
            r#": "${{{key}:=$P05_MISE_DIRS/{dir}}}"
export {key}
private_path "${key}""#
        )
        .unwrap();
    }
    write(&guard_file, &guard, 0o600);
    write(
        &guarded_exec,
        "#!/bin/sh\nset -eu\n. \"$P05_FIXTURE_GUARD\"\nexec \"$@\"\n",
        0o755,
    );
    let mise = private.join("bin/mise");
    write(
        &mise,
        "#!/bin/sh\nset -eu\n. \"$P05_FIXTURE_GUARD\"\nexec \"$P05_REAL_MISE\" \"$@\"\n",
        0o755,
    );
    command
        .env("DOTS_MISE_BIN", &mise)
        .env("SETUP_MISE_BIN", mise);
}

fn dots(world: &World) -> FixtureCommand<'_> {
    let mut command = world.dots();
    isolate(world, &mut command);
    command
}

fn fixture_command<'a>(world: &'a World, program: &Path) -> FixtureCommand<'a> {
    let mut command = world.command(program);
    isolate(world, &mut command);
    command
}

fn test_mise(world: &World) -> PathBuf {
    let fake = world.home().join("bin/mise");
    write(
        &fake,
        r#"#!/bin/sh
. "$P05_FIXTURE_GUARD"
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

fn fake(world: &World) -> FixtureCommand<'_> {
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

fn complete(world: &World) -> FixtureCommand<'_> {
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
    let script = format!("#!/bin/sh\nset -eu\n. \"$P05_FIXTURE_GUARD\"\n{body}\n");
    write(
        &world.root().join("dots/hooks/bootstrap.d").join(name),
        &script,
        mode,
    );
}

fn run(command: &mut FixtureCommand<'_>) -> Output {
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
    let bin = world.home().join("mise-fixture/bin");
    write(&bin.join("ssh"), "#!/bin/sh\nexit 92\n", 0o755);
    write(
        &world.root().join("config.toml"),
        &format!("{MINIMAL}dots_source_repo = \"https://example.invalid/dotfiles\"\n"),
        0o644,
    );
    let output = run(fake(&world).args([
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
    let rejected = run(fake(&world).args([
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
    let generated = run(fixture_command(&world, &keygen).arg("-o").arg(&identity));
    assert_success(&generated);
    let public = run(fixture_command(&world, &keygen).arg("-y").arg(&identity));
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
fn new_hook_bundle_descriptors_and_folded_outcomes_are_private() {
    use std::io::Write as _;
    use std::process::Stdio;

    let world = World::new();
    minimal(&world);
    write(
        &world.root().join("config.toml"),
        &format!(
            r#"{MINIMAL}
[env]
DOTS_BUNDLE_FD = "999"
DOTS_PRIVATE_READY = "0"
DOTS_RESTORED = "1"
"#
        ),
        0o644,
    );
    write(&world.root().join("config.laptop.toml"), "", 0o644);
    write(
        &world.root().join("dots/exceptions.tsv"),
        "base\tfixture-recipe\n",
        0o644,
    );
    write(
        &world.root().join("dots/exceptions/fixture-recipe"),
        r#"#!/bin/sh
set -eu
. "$P05_FIXTURE_GUARD"
test "$#" = 1
test "$1" = --install-only
test -z "${DOTS_BUNDLE_FD+x}"
test -z "${DOTS_PRIVATE_READY+x}"
test -z "${DOTS_RESTORED+x}"
touch "$HOME/recipe-ready"
exit 4
"#,
        0o755,
    );
    let bin = world.home().join("mise-fixture/bin");
    let age = bin.join("age");
    write(
        &age,
        r#"#!/bin/sh
. "$P05_FIXTURE_GUARD"
[ "$1" = --decrypt ] && [ -f "$3" ] && [ -f "$5" ] || exit 91
printf '%s' '{"version":1,"secrets":{"github_token":"fixture-token\nsecond line"}}'
"#,
        0o755,
    );
    let hook_body = r#"
        test -f "$HOME/recipe-ready"
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
    assert!(
        stdout(&output).contains("skipped: Installer exception: fixture-recipe"),
        "{output:?}"
    );
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
    assert!(report.contains("Installer exception: fixture-recipe"));
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
    let pre_dotfiles = world.base().join("pre-dotfiles");
    write(
        &pre_dotfiles,
        r#"#!/bin/sh
set -eu
. "$P05_FIXTURE_GUARD"
test -z "${MISE_ENV:-}"
test "$MISE_HISTORY_ENABLED" = 0
test ! -e "$HOME/applied"
SETUP_PROFILE=laptop SETUP_MACHINE_ID=fixture-host exec "$DOTS_MISE_BIN" x -- "$P05_GUARDED_EXEC" dots bootstrap persist-selected
"#,
        0o755,
    );
    let config = format!(
        r#"{MINIMAL}
[bootstrap.hooks]
pre-dotfiles = {pre_dotfiles:?}
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
        .env("MISE_ENV", "server,external")
        .env("MISE_TRUSTED_CONFIG_PATHS", world.root())
        .env(
            "MISE_SYSTEM_CONFIG_FILE",
            world.home().join("empty-system.toml"),
        )
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
    let bin = world.home().join("mise-fixture/bin");
    write(
        &bin.join("age-keygen"),
        r#"#!/bin/sh
. "$P05_FIXTURE_GUARD"
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
    let complete = || run(complete(&world).env("SETUP_AGE_IDENTITY", &recovery));
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
fn new_bootstrap_validates_the_url_mise_uses_after_extracting_the_ref() {
    let world = World::new();
    minimal(&world);
    let remote = world.bare_remote("source");
    let url = format!("file://{}", remote.display());
    let config = world.root().join("config.toml");
    write(
        &config,
        &format!("{MINIMAL}dots_source_repo = {url:?}\n"),
        0o644,
    );
    let marker = "synthetic-query-value";
    for (flag, suffix) in [
        ("--from", format!("ref=main&access_token={marker}")),
        ("--adopt", format!("password={marker}&ref=main")),
        ("--from", format!("ref=main&to%6Ben={marker}")),
        ("--adopt", format!("ref=bad?token={marker}")),
        ("--from", "ref=HEAD".to_owned()),
        ("--from", "ref=-option".to_owned()),
        ("--from", "ref=".to_owned()),
    ] {
        let input = format!("{url}?{suffix}");
        let output = run(fake(&world).env("NATIVE_FIXTURE_STATUS", "89").args([
            "bootstrap",
            flag,
            &input,
            "--dry-run",
        ]));
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains(&input) && !stderr.contains(marker),
            "{stderr}"
        );
    }

    let effective = format!("{url}?view=full&compression=off");
    write(
        &config,
        &format!("{MINIMAL}dots_source_repo = {effective:?}\n"),
        0o644,
    );
    let input = format!("{url}?view=full&ref=old&compression=off&ref=release/v1");
    let output = run(fake(&world).args(["bootstrap", "--from", &input, "--dry-run"]));
    assert_success(&output);
    assert!(
        stdout(&output).contains(&format!("--from\n{effective}&ref=release/v1\n")),
        "{output:?}"
    );
    let input = format!("{effective}&ref=release/v1~1");
    let output = run(fake(&world).args(["bootstrap", "--from", &input, "--dry-run"]));
    assert_success(&output);
    assert!(
        stdout(&output).contains(&format!("--from\n{input}\n")),
        "{output:?}"
    );

    write(
        &config,
        &format!("{MINIMAL}dots_source_repo = {url:?}\n"),
        0o644,
    );
    let input = format!("{url}?ref=release/v1");
    let output = run(dots(&world)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .args(["bootstrap", "--from", &input, "--from-dir"])
        .arg(world.base().join("checkout"))
        .arg("--dry-run"));
    assert_success(&output);
    assert!(
        stdout(&output).contains(&format!("git clone {url}")),
        "{output:?}"
    );
    assert!(
        stdout(&output).contains("checkout release/v1 --"),
        "{output:?}"
    );
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

#[test]
fn new_bootstrap_update_checks_and_uses_the_global_config_files_checkout() {
    let world = World::new();
    minimal(&world);
    let expected = format!("file://{}", world.bare_remote("expected").display());
    let foreign = format!("file://{}", world.bare_remote("foreign").display());
    let alternate = world.base().join("alternate");
    let global = alternate.join("alternate.toml");
    let local = world.root().join("config.toml");
    write(&global, MINIMAL, 0o644);
    write(
        &local,
        &format!("{MINIMAL}dots_source_repo = {expected:?}\n"),
        0o644,
    );
    let git = |checkout: &Path, args: &[&str]| {
        assert_success(&run(world.git().current_dir(checkout).args(args)));
    };
    for (dir, upstream) in [
        (world.root(), expected.as_str()),
        (&alternate, foreign.as_str()),
    ] {
        git(dir, &["init", "--initial-branch=main"]);
        git(dir, &["add", "--all"]);
        git(dir, &["commit", "-m", "Seed fixture"]);
        git(dir, &["remote", "add", "origin", &expected]);
        git(dir, &["remote", "add", "upstream", upstream]);
        git(dir, &["update-ref", "refs/remotes/upstream/main", "HEAD"]);
        git(dir, &["branch", "--set-upstream-to=upstream/main"]);
    }

    for setting in ["MISE_GLOBAL_CONFIG_FILE", "MISE_CONFIG_FILE"] {
        let output = run(fake(&world)
            .env(setting, &global)
            .env("NATIVE_FIXTURE_STATUS", "89")
            .args(["bootstrap", "--update", "--dry-run"]));
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("outside the expected bootstrap repository"),
            "{output:?}"
        );
    }

    let output = run(fake(&world)
        .env("MISE_GLOBAL_CONFIG_FILE", &global)
        .env("NATIVE_FIXTURE_STATUS", "89")
        .args(["bootstrap", "--adopt", &expected, "--update", "--dry-run"]));
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");

    git(&alternate, &["remote", "set-url", "upstream", &expected]);
    let output = run(dots(&world)
        .env("MISE_GLOBAL_CONFIG_FILE", &global)
        .env("MISE_CONFIG_FILE", &local)
        .env("MISE_CD", world.base().join("absent"))
        .env("MISE_TRUSTED_CONFIG_PATHS", &alternate)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .args(["bootstrap", "--update", "--dry-run", "--only", "dotfiles"]));
    assert_success(&output);
    assert!(
        stdout(&output).contains(&format!("git -C {} pull --ff-only", alternate.display())),
        "{output:?}"
    );
}

#[test]
fn new_native_restore_keeps_the_restore_id_and_skips_services() {
    let world = World::new();
    minimal(&world);
    let output = run(fake(&world)
        .env("MISE_ENV", "laptop,server")
        .env("SETUP_PROFILE", "laptop")
        .env("SETUP_MACHINE_ID", "other-host")
        .args([
            "bootstrap",
            "--restore",
            "target-host",
            "-E",
            "laptop,host-other-host",
        ]));
    assert_success(&output);
    let child = stdout(&output);
    assert!(child.contains("--skip\nservices\n"), "{child}");
    assert!(child.contains("--env\nhost-target-host\n"), "{child}");
    assert!(!child.contains("host-other-host"), "{child}");
    assert!(!child.contains("--restore"), "{child}");

    write(
        &world.root().join("config.local.toml"),
        "[env]\nSETUP_PROFILE = \"laptop\"\nSETUP_MACHINE_ID = \"enrolled-host\"\n",
        0o600,
    );
    let output = run(fake(&world).args(["bootstrap", "--restore", "target-host"]));
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("differs from this enrolled machine"));
}

#[test]
fn new_cancelled_bundle_probe_keeps_its_streams_private() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let world = World::new();
    let key = world.home().join("identity");
    let input = world.home().join("bundle.age");
    let ready = world.home().join("probe-ready");
    write(&key, "fixture identity", 0o600);
    write(&input, "fixture ciphertext", 0o600);
    let mut command = dots(&world);
    write(
        &world.home().join("mise-fixture/bin/age"),
        r#"#!/bin/sh
. "$P05_FIXTURE_GUARD"
printf "synthetic-capture-sensitive-marker\n"
printf "synthetic-capture-sensitive-marker\n" >&2
touch "$HOME/probe-ready"
exec sleep 30
"#,
        0o755,
    );
    let mut child = command
        .args(["bundle", "check", "--bundle"])
        .arg(input)
        .arg("--identity")
        .arg(key)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() && Instant::now() < deadline && child.try_wait().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(15));
    }
    if !ready.exists() {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!("age probe did not start: {output:?}");
    }
    // SAFETY: this PID is the live, unreaped dots child from this fixture.
    assert_eq!(
        unsafe { nix::libc::kill(child.id() as i32, nix::libc::SIGINT) },
        0
    );
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let marker = "synthetic-capture-sensitive-marker";
    assert!(!stdout(&output).contains(marker));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(marker));
    let reports = world.home().join(".local/state/dots/reports");
    let report = fs::read_dir(reports)
        .unwrap()
        .find_map(|entry| {
            let path = entry.unwrap().path();
            (path.extension()? == "json").then(|| fs::read_to_string(path).unwrap())
        })
        .unwrap();
    assert!(!report.contains(marker));
    let parsed: serde_json::Value = serde_json::from_str(&report).unwrap();
    assert_eq!(parsed["cancelled"], true);
}

#[test]
#[ignore = "requires the P-06 history implementation"]
fn new_native_bootstrap_restores_and_enrolls_with_separate_identities() {
    use std::collections::BTreeSet;
    use std::io::Write as _;
    use std::process::Stdio;

    const PAYLOAD: &[u8] = b"synthetic history input for bootstrap\n";
    let age = program("age").expect("age on PATH");
    let keygen = program("age-keygen").expect("age-keygen on PATH");
    let seed = World::new();
    let world = World::new();
    let remote = world.bare_remote("encrypted-history");
    let origin = format!("file://{}", remote.display());
    let tracked = format!(
        "{MINIMAL}dots_state_repo = {origin:?}\n\n[dotfiles]\n\
        \"~/recovered-setting\" = {{ mode = \"track\", encrypt = true, variants = [{{ profile = \"host-fixture-host\" }}] }}\n"
    );
    for fixture in [&seed, &world] {
        minimal(fixture);
        write(&fixture.root().join("config.toml"), &tracked, 0o644);
        write(&fixture.root().join("config.laptop.toml"), "", 0o644);
        for program in ["ssh", "curl", "launchctl", "systemctl"] {
            write(
                &fixture.home().join("mise-fixture/bin").join(program),
                "#!/bin/sh\necho unexpected fixture transport or live service call >&2\nexit 99\n",
                0o755,
            );
        }
    }
    let key = |name: &str| {
        let path = world.home().join(name);
        assert_success(&run(fixture_command(&world, &keygen).arg("-o").arg(&path)));
        let recipient = run(fixture_command(&world, &keygen).arg("-y").arg(&path));
        assert_success(&recipient);
        (path, stdout(&recipient).trim().to_owned())
    };
    let (recovery, recovery_recipient) = key("recovery.key");
    let (bundle_identity, bundle_key) = key("bundle.key");
    let (_, extra_recipient) = key("existing.key");
    let (_, bundle_recipient) = key("bundle-recipient.key");
    let recovery_before = fs::read(&recovery).unwrap();
    let bundle_before = fs::read(&bundle_identity).unwrap();

    write(
        &seed.root().join("config.local.toml"),
        &format!(
            "[settings.history]\nsync = \"manual\"\n\n[history.encryption]\nrecipients = [{recovery_recipient:?}]\n"
        ),
        0o600,
    );
    write(
        &seed.home().join("recovered-setting"),
        std::str::from_utf8(PAYLOAD).unwrap(),
        0o640,
    );
    let seed_mise = seed.home().join("mise-fixture/bin/mise");
    assert_success(&run(fixture_command(&seed, &seed_mise)
        .env("MISE_TRUSTED_CONFIG_PATHS", seed.root())
        .env("MISE_ENV", "laptop,host-fixture-host")
        .args(["bootstrap", "dotfiles", "save"])));
    let store = seed.home().join("mise-fixture/state/history/repo.git");
    assert_success(&run(seed
        .git()
        .env("GIT_ALLOW_PROTOCOL", "file")
        .arg("--git-dir")
        .arg(&store)
        .args(["push", "--"])
        .arg(&remote)
        .arg("HEAD:refs/heads/host-fixture-host")));
    let refs = || {
        let output = run(world.git().arg("--git-dir").arg(&remote).arg("show-ref"));
        assert_success(&output);
        output.stdout
    };
    let before = refs();
    let remote_objects = run(world.git().arg("--git-dir").arg(&remote).args([
        "cat-file",
        "--batch-all-objects",
        "--batch",
    ]));
    assert_success(&remote_objects);
    assert!(
        remote_objects
            .stdout
            .windows(b"mise-encrypted-file-v1".len())
            .any(|bytes| bytes == b"mise-encrypted-file-v1")
    );
    assert!(
        !remote_objects
            .stdout
            .windows(PAYLOAD.len())
            .any(|bytes| bytes == PAYLOAD)
    );

    let ciphertext = world.home().join("bootstrap.age");
    let mut encrypt = fixture_command(&world, &age)
        .args(["-r", &bundle_key, "-o"])
        .arg(&ciphertext)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    encrypt
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({
                "version": 1,
                "secrets": {"TEST": "synthetic-bundle-value"},
                "recipients": [&bundle_recipient],
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    assert!(encrypt.wait().unwrap().success());

    write(
        &world.root().join("config.local.toml"),
        &format!(
            r#"
[env]
SETUP_HISTORY_ORIGIN = {origin:?}
SETUP_AGE_IDENTITY = {recovery:?}

[vars]
bootstrap_bundle_identity = {bundle_identity:?}

[history.encryption]
recipients = [{extra_recipient:?}]

[bootstrap.services.unrelated-fixture]
command = "false"
"#
        ),
        0o600,
    );
    let base = world.root().parent().unwrap();
    let bin = world.home().join("mise-fixture/bin");
    write(&bin.join("pgrep"), "#!/bin/sh\nexit 1\n", 0o755);
    let wrapper = world.home().join("watcher-fixture-mise");
    write(
        &wrapper,
        r#"#!/bin/sh
set -eu
. "$P05_FIXTURE_GUARD"
printf '%s\n' "$*" >> "$HOME/native-calls"
case "$1 ${2-} ${3-}" in
    'bootstrap services apply')
        [ "$#" -eq 4 ] && [ "$4" = '--yes' ] && [ "$MISE_HISTORY_ENABLED" = 0 ] || exit 95
        test ! -s "$MISE_SYSTEM_CONFIG_FILE"
        cp "$MISE_GLOBAL_CONFIG_FILE" "$HOME/scoped-services.toml"
        exit 0
        ;;
    'dot status --json')
        test -f "$HOME/scoped-services.toml"
        printf '%s\n' '{"history":{"watcher":"running"}}'
        exit 0
        ;;
    'bootstrap services '*|'bootstrap dotfiles watch')
        echo unexpected watcher or services invocation >&2
        exit 97
        ;;
esac
if [ "$1" = bootstrap ] && [ "${2-}" = --skip-dirty ]; then
    case " $* " in *" --skip services "*) ;; *) exit 95 ;; esac
    [ "$MISE_HISTORY_ENABLED" = 0 ] || exit 95
fi
if [ "$1 ${2-} ${3-} ${4-}" = '--yes bootstrap dotfiles origin' ]; then
    printf 'override=%s\n' "${MISE_HISTORY_ENABLED-unset}" >> "$HOME/native-origin.stderr"
    "$P05_MISE_DIRS/bin/mise" "$@" 2>> "$HOME/native-origin.stderr"
    exit $?
fi
exec "$P05_MISE_DIRS/bin/mise" "$@"
"#,
        0o755,
    );

    let mut command = dots(&world);
    let target_dots = command.get_program().to_owned();
    let callback = base.join("guarded-callback");
    write(
        &callback,
        r#"#!/bin/sh
set -eu
. "$P05_FIXTURE_GUARD"
test ! -d "$MISE_STATE_DIR/history/repo.git"
exec "$P05_DOTS" bootstrap complete
"#,
        0o755,
    );
    write(
        &world.root().join("config.toml"),
        &format!("{tracked}\n[tasks.bootstrap]\nrun = {callback:?}\nraw = true\n"),
        0o644,
    );
    let output = run(command
        .env("DOTS_MISE_BIN", &wrapper)
        .env("SETUP_MISE_BIN", &wrapper)
        .env("P05_DOTS", target_dots)
        .env("MISE_TRUSTED_CONFIG_PATHS", world.root())
        .env("MISE_AUTO_INSTALL", "0")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("SETUP_PROFILE", "laptop")
        .env("SETUP_MACHINE_ID", "fixture-host")
        .args([
            "bootstrap",
            "--yes",
            "--restore",
            "fixture-host",
            "--bundle",
        ])
        .arg(&ciphertext));
    assert!(
        output.status.success(),
        "{output:?}\n{}\n{}",
        fs::read_to_string(world.home().join("native-origin.stderr")).unwrap_or_default(),
        fs::read_to_string(world.home().join("native-calls")).unwrap_or_default(),
    );
    assert!(
        stdout(&output).contains("completed: Restore private settings"),
        "{output:?}"
    );
    assert!(
        stdout(&output).contains("completed: Encrypted local history"),
        "{output:?}"
    );
    assert!(!stdout(&output).contains("synthetic-bundle-value"));
    let restored = world.home().join("recovered-setting");
    assert_eq!(fs::read(&restored).unwrap(), PAYLOAD);
    assert_eq!(
        fs::metadata(restored).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let machine = world.home().join(".config/age/keys.txt");
    assert_eq!(
        fs::metadata(&machine).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(machine.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(fs::read(&recovery).unwrap(), recovery_before);
    assert_eq!(fs::read(&bundle_identity).unwrap(), bundle_before);
    let machine_recipient = run(fixture_command(&world, &keygen).arg("-y").arg(&machine));
    assert_success(&machine_recipient);
    let local = fs::read_to_string(world.root().join("config.local.toml")).unwrap();
    let config = local.parse::<toml_edit::DocumentMut>().unwrap();
    assert_eq!(
        config["env"]["SETUP_AGE_IDENTITY"].as_str(),
        recovery.to_str()
    );
    assert_eq!(
        config["vars"]["bootstrap_bundle_identity"].as_str(),
        bundle_identity.to_str()
    );
    assert_eq!(
        config["settings"]["age"]["identity_files"]
            .as_array()
            .unwrap()
            .get(0)
            .unwrap()
            .as_str(),
        machine.to_str()
    );
    assert_eq!(
        config["settings"]["history"]["sync"].as_str(),
        Some("manual")
    );
    assert_eq!(
        config["history"]["origin"]["branch"].as_str(),
        Some("host-fixture-host")
    );
    assert_eq!(
        config["history"]["origin"]["url"].as_str(),
        Some(origin.as_str())
    );
    let recipients: BTreeSet<_> = config["history"]["encryption"]["recipients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap())
        .collect();
    assert_eq!(
        recipients,
        [
            &*recovery_recipient,
            &*bundle_recipient,
            &*extra_recipient,
            stdout(&machine_recipient).trim(),
        ]
        .into_iter()
        .collect()
    );
    let scoped = fs::read_to_string(world.home().join("scoped-services.toml"))
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    let services = scoped["bootstrap"]["services"].as_table().unwrap();
    assert_eq!(services.len(), 1);
    assert_eq!(
        services["mise-history"]["builtin"].as_str(),
        Some("history-watch")
    );
    assert_eq!(
        services["mise-history"]["environment"]["MISE_CONFIG_DIR"].as_str(),
        world.root().to_str()
    );
    assert!(
        config["bootstrap"]["services"]
            .as_table()
            .unwrap()
            .contains_key("unrelated-fixture")
    );
    let trace = fs::read_to_string(world.home().join("native-calls")).unwrap();
    let restore = trace.find("bootstrap dotfiles pull").unwrap();
    let activate = trace.find("bootstrap services apply --yes").unwrap();
    let verify = trace.find("dot status --json").unwrap();
    assert!(restore < activate && activate < verify);
    assert_eq!(trace.matches("bootstrap services apply").count(), 1);
    assert_eq!(refs(), before);

    let command = || {
        let mut cmd = dots(&world);
        cmd.env("DOTS_MISE_BIN", &wrapper)
            .env("SETUP_MISE_BIN", &wrapper)
            .env("MISE_TRUSTED_CONFIG_PATHS", world.root())
            .env("MISE_AUTO_INSTALL", "0")
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("SETUP_PROFILE", "laptop")
            .env("SETUP_MACHINE_ID", "fixture-host");
        cmd
    };
    let machine_before = fs::read(&machine).unwrap();
    let standalone = run(command()
        .args([
            "enroll-history",
            "--profile",
            "laptop",
            "--machine-id",
            "fixture-host",
            "--identity",
        ])
        .arg(&machine));
    assert_success(&standalone);
    let trace = fs::read_to_string(world.home().join("native-calls")).unwrap();
    assert_eq!(trace.matches("bootstrap services apply").count(), 1);
    assert_eq!(trace.matches("dot status --json").count(), 1);

    let path = world.root().join("config.local.toml");
    let mut config = fs::read_to_string(&path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    config["env"]
        .as_table_mut()
        .unwrap()
        .remove("SETUP_AGE_IDENTITY");
    write(&path, &config.to_string(), 0o600);
    let enroll = run(command()
        .env("SETUP_AGE_IDENTITY", &recovery)
        .env("DOTS_BOOTSTRAP_BUNDLE", &ciphertext)
        .args(["bootstrap", "complete"]));
    assert_success(&enroll);
    assert!(
        stdout(&enroll).contains("completed: Encrypted local history"),
        "{enroll:?}"
    );
    let config = fs::read_to_string(path)
        .unwrap()
        .parse::<toml_edit::DocumentMut>()
        .unwrap();
    assert_eq!(
        config["env"]["SETUP_AGE_IDENTITY"].as_str(),
        recovery.to_str()
    );
    assert_eq!(fs::read(&machine).unwrap(), machine_before);
    let trace = fs::read_to_string(world.home().join("native-calls")).unwrap();
    assert_eq!(trace.matches("bootstrap services apply").count(), 2);
    assert_eq!(trace.matches("dot status --json").count(), 2);
    assert_eq!(refs(), before);
}

#[test]
fn new_local_native_bootstrap_does_not_checkpoint_a_plaintext_track() {
    let world = World::new();
    minimal(&world);
    let secret = world.home().join("plaintext-fixture");
    write(&secret, "synthetic pre-bootstrap content\n", 0o600);
    write(
        &world.root().join("config.toml"),
        &format!("{MINIMAL}\n[dotfiles]\n\"~/plaintext-fixture\" = {{ mode = \"track\" }}\n"),
        0o644,
    );
    write(&world.root().join("config.laptop.toml"), "", 0o644);
    for program in ["ssh", "curl", "launchctl", "systemctl"] {
        write(
            &world.home().join("mise-fixture/bin").join(program),
            "#!/bin/sh\nexit 99\n",
            0o755,
        );
    }
    let output = run(dots(&world)
        .env("MISE_TRUSTED_CONFIG_PATHS", world.root())
        .env("SETUP_PROFILE", "laptop")
        .env("GIT_ALLOW_PROTOCOL", "file")
        .args(["bootstrap", "--yes", "--only", "dotfiles"]));
    assert_success(&output);
    assert_eq!(
        fs::read_to_string(secret).unwrap(),
        "synthetic pre-bootstrap content\n"
    );
    assert!(
        !world
            .home()
            .join("mise-fixture/state/history/repo.git")
            .exists()
    );
}
