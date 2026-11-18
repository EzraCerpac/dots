mod support;

use std::env;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Output, Stdio};

use support::{FixtureCommand, World};

const CONFIG: &str = "[vars]\ndots_profiles = \"laptop server\"\n";
const HOOKS: &str = "dots/hooks/update.d";

struct UpdateFixture {
    world: World,
    path: std::ffi::OsString,
}

impl UpdateFixture {
    fn new() -> Self {
        let world = World::new();
        fs::write(world.root().join("config.toml"), CONFIG).unwrap();
        fs::write(world.root().join("config.laptop.toml"), "").unwrap();
        fs::remove_file(world.root().join("dots/exceptions.tsv")).unwrap();
        fs::remove_dir_all(world.root().join(HOOKS)).unwrap();
        fs::create_dir(world.home().join("tmp")).unwrap();

        let bin = world.home().join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let mise = bin.join("mise");
        fs::write(&mise, include_bytes!("contract_update_fixtures/mise")).unwrap();
        fs::set_permissions(mise, fs::Permissions::from_mode(0o755)).unwrap();

        // Keep project and user shims, other dots releases, and package
        // managers off PATH. Node and jq are only used by the v0.4.0 oracle.
        let caller_path = env::var_os("PATH").unwrap_or_default();
        for tool in ["git", "node", "jq"] {
            if let Some(program) = env::split_paths(&caller_path)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(tool))
                .find(|path| {
                    path.is_file() && path.metadata().unwrap().permissions().mode() & 0o111 != 0
                })
            {
                symlink(program, bin.join(tool)).unwrap();
            }
        }
        let path =
            env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
        Self { world, path }
    }

    fn dots(&self) -> FixtureCommand<'_> {
        let mut command = self.world.dots();
        command
            .env("PATH", &self.path)
            .env("LC_ALL", "C")
            .env("TMPDIR", self.world.home().join("tmp"))
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("SETUP_PROFILE", "laptop")
            .env("SETUP_MACHINE_ID", "update-fixture")
            .env("DOTS_MISE_BIN", self.world.home().join(".local/bin/mise"));
        command
    }

    fn up(&self, mode: &str) -> FixtureCommand<'_> {
        let mut command = self.dots();
        command.args([mode, "up"]);
        command
    }

    fn write(&self, path: &str, contents: &str) {
        fs::write(self.world.root().join(path), contents).unwrap();
    }

    fn hook(&self, name: &str, body: &str) {
        self.script(&format!("{HOOKS}/{name}"), body);
    }

    fn script(&self, path: &str, body: &str) {
        script(&self.world.root().join(path), body);
    }

    fn read_home(&self, name: &str) -> String {
        fs::read_to_string(self.world.home().join(name)).unwrap()
    }

    fn assert_no_package_install(&self) {
        let calls = match fs::read_to_string(self.world.home().join("mise-calls")) {
            Ok(calls) => calls,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!("cannot read mock mise calls: {error}"),
        };
        assert!(
            !calls
                .lines()
                .any(|line| line.starts_with("bootstrap packages apply")
                    && !line
                        .split_whitespace()
                        .any(|argument| argument == "--dry-run")),
            "{calls}"
        );
    }
}

fn script(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn run(command: &mut FixtureCommand<'_>) -> Output {
    command.output().unwrap()
}

fn output_text(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn exit(output: &Output, code: i32) -> String {
    let text = output_text(output);
    assert_eq!(output.status.code(), Some(code), "{text}");
    text
}

fn mentions(text: &str, parts: &[&str]) {
    for part in parts {
        assert!(text.contains(part), "missing {part:?} in:\n{text}");
    }
}

fn ordered(text: &str, parts: &[&str]) {
    let mut rest = text;
    for part in parts {
        let Some((_, after)) = rest.split_once(part) else {
            panic!("missing ordered {part:?} in:\n{text}");
        };
        rest = after;
    }
}

#[test]
#[ignore = "P-04"]
fn unchanged_invalid_update_configuration_stops_dependent_work() {
    // The legacy renderer nonce framed the startup failure. The CLI diagnostic
    // and the absence of dependent work expose the failure without that protocol.
    let f = UpdateFixture::new();
    f.hook("10-unexpected", "touch \"$HOME/unexpected\"");
    let output = run(f.up("--verbose").env("FIXTURE_CONFIG_FAILURE", "1"));
    assert!(
        matches!(output.status.code(), Some(1 | 2)),
        "{}",
        output_text(&output)
    );
    mentions(
        &output_text(&output),
        &["invalid fixture setting", "dots up"],
    );
    let calls = f.read_home("mise-calls");
    assert!(
        !calls
            .lines()
            .any(|line| line == "install" || line.starts_with("upgrade"))
    );
    assert!(!f.world.home().join("unexpected").exists());
}

#[test]
#[ignore = "P-04"]
fn unchanged_manager_upgrade_is_reported_for_the_next_run() {
    let f = UpdateFixture::new();
    f.write(
        "config.toml",
        &format!("{CONFIG}\n[tools]\n\"github:EzraCerpac/dots\" = \"latest\"\n"),
    );
    let replacement = f.world.home().join("new-manager");
    script(
        &replacement.join("bin/dots"),
        "printf '%s\\n' 'dots 99.0.0 (other)'",
    );
    // v0.4.0 recognizes another release by its support tree and VERSION file.
    fs::create_dir(replacement.join("lib")).unwrap();
    fs::write(replacement.join("lib/dots-manager.sh"), "").unwrap();
    fs::write(replacement.join("VERSION"), "99.0.0\n").unwrap();
    f.hook(
        "10-cleanup",
        "printf '%s' \"$HOMEBREW_NO_INSTALL_CLEANUP\" > \"$HOME/cleanup\"",
    );

    let version = exit(&run(f.dots().arg("--version")), 0);
    let version = version.split_whitespace().nth(1).unwrap();
    let text = exit(&run(&mut f.up("--verbose")), 0);
    mentions(
        &text,
        &[&format!("dots {version} -> 99.0.0, used from the next run")],
    );
    assert!(f.world.home().join("manager-installed").is_file());
    assert_eq!(f.read_home("cleanup"), "1");
}

#[test]
#[ignore = "P-04"]
fn new_update_hook_and_recipe_keep_the_running_dots_after_mise_sets_path() {
    let f = UpdateFixture::new();
    let competing = f.world.home().join("competing");
    script(&competing.join("dots"), "printf '%s\\n' 'competing dots'");
    f.hook(
        "10-version",
        "test \"$FIXTURE_MISE_PATH_APPLIED\" = 1; dots --version > \"$HOME/hook-version\"",
    );
    f.script(
        "dots/exceptions/version",
        "test \"$FIXTURE_MISE_PATH_APPLIED\" = 1; dots --version > \"$HOME/recipe-version\"",
    );
    f.write("dots/exceptions.tsv", "base\tversion\n");

    let expected = run(f.dots().arg("--version"));
    exit(&expected, 0);
    exit(&run(f.up("--plain").env("FIXTURE_MISE_PATH", competing)), 0);
    for name in ["hook-version", "recipe-version"] {
        assert_eq!(
            fs::read(f.world.home().join(name)).unwrap(),
            expected.stdout,
            "{name}"
        );
    }
}

#[test]
#[ignore = "P-04"]
fn unchanged_package_managed_mise_is_not_self_updated() {
    let f = UpdateFixture::new();
    let lib = f.world.home().join(".local/lib");
    fs::create_dir(&lib).unwrap();
    fs::write(lib.join(".disable-self-update"), "").unwrap();
    let text = exit(&run(&mut f.up("--verbose")), 0);
    mentions(&text, &["mise", "package manager"]);
    assert!(
        !f.read_home("mise-calls")
            .lines()
            .any(|line| line.starts_with("self-update"))
    );
}

#[test]
#[ignore = "P-04"]
fn unchanged_update_hooks_run_in_name_order_with_environment_and_outcomes() {
    let f = UpdateFixture::new();
    for (name, code) in [
        ("50-after-failure", 0),
        ("40-broken", 7),
        ("20-deferred_step", 3),
        ("45-laptop-only@laptop", 0),
        ("46-server-only@server", 0),
        ("70 odd name", 0),
        ("60-not-executable", 0),
    ] {
        f.hook(
            name,
            &format!("echo '{name}' >> \"$HOME/hooks\"\nexit {code}"),
        );
    }
    f.hook(
        "30-skipped.sh",
        "echo 30-skipped.sh >> \"$HOME/hooks\"; echo 'fixture requested skip'; exit 4",
    );
    f.hook(
        "10-first",
        r#"echo 10-first >> "$HOME/hooks"
printf '%s\n' "$DOTS_ROOT" "$DOTS_PROFILE" "$DOTS_TOOLS_OK" "$DOTS_TOOLS_UPGRADED" "$PWD" > "$HOME/hook-env"
grep -q '^upgrade' "$HOME/mise-calls"
if grep -q '^self-update' "$HOME/mise-calls"; then exit 1; fi"#,
    );
    fs::set_permissions(
        f.world.root().join(HOOKS).join("60-not-executable"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fs::create_dir(f.world.root().join(HOOKS).join("80-directory")).unwrap();

    let text = exit(&run(&mut f.up("--verbose")), 1);
    ordered(
        &text,
        &[
            "deferred step",
            "deferred",
            "skipped",
            "fixture requested skip",
            "broken",
            "exit 7",
        ],
    );
    assert_eq!(
        f.read_home("hooks"),
        "10-first\n20-deferred_step\n30-skipped.sh\n40-broken\n45-laptop-only@laptop\n50-after-failure\n"
    );
    let root = f.world.root().display();
    assert_eq!(
        f.read_home("hook-env"),
        format!("{root}\nlaptop\n1\n1\n{root}\n")
    );
    assert!(
        f.read_home("mise-calls")
            .lines()
            .any(|line| line.starts_with("self-update"))
    );
}

#[test]
#[ignore = "P-04"]
fn unchanged_hook_changes_are_visible_in_plain_output() {
    // The old renderer's event descriptor is gone. Check the printed changes.
    let f = UpdateFixture::new();
    f.hook(
        "10-Changes",
        r#"printf 'plugin-a\told-abc\tnew-def\nbogus-no-tabs\n\nbogus-extra\ta\tb\tc\n' >> "$DOTS_CHANGES"
printf 'plugin-b\tnot installed\tadded-xyz\nplugin-c\tbefore-gone\t\n' >> "$DOTS_CHANGES""#,
    );
    let text = exit(&run(&mut f.up("--plain")), 0);
    ordered(
        &text,
        &[
            "plugin-a",
            "old-abc",
            "new-def",
            "plugin-b",
            "added-xyz",
            "plugin-c",
            "before-gone",
        ],
    );
    assert!(
        !text.contains("bogus-no-tabs") && !text.contains("bogus-extra"),
        "{text}"
    );
}

#[test]
#[ignore = "P-04"]
fn unchanged_hook_labels_and_profiles_are_visible_when_run() {
    let f = UpdateFixture::new();
    for name in [
        "b-two",
        "A-one.sh",
        "-dash",
        "skip.me~",
        "10-Login-shell.sh",
        "20_a.b.c",
        "30-Laptop-only@laptop",
        "31-Server-only.sh@server",
        "32-backup@laptop~",
    ] {
        f.hook(name, &format!("echo '{name}' >> \"$HOME/selected-hooks\""));
    }
    for (profile, label, name) in [
        ("laptop", "Laptop only", "30-Laptop-only@laptop"),
        ("server", "Server only", "31-Server-only.sh@server"),
    ] {
        let text = exit(&run(f.up("--verbose").env("SETUP_PROFILE", profile)), 0);
        ordered(&text, &["Login shell", "a.b", label, "A one", "b two"]);
        assert_eq!(
            f.read_home("selected-hooks"),
            format!("10-Login-shell.sh\n20_a.b.c\n{name}\nA-one.sh\nb-two\n")
        );
        fs::remove_file(f.world.home().join("selected-hooks")).unwrap();
    }
    fs::remove_dir_all(f.world.root().join(HOOKS)).unwrap();
    exit(&run(&mut f.up("--verbose")), 0);
    assert!(!f.world.home().join("selected-hooks").exists());
}

// Also covers HookListingTests.test_unknown_profile_suffix_fails_without_listing.
// The private listing helper's exit code and tab-separated output disappeared
// with the helper. Assert the public update failure and no hook execution.
#[test]
#[ignore = "P-04"]
fn unchanged_unknown_hook_profile_prevents_all_hooks_in_that_directory() {
    let f = UpdateFixture::new();
    f.hook("10-valid", "touch \"$HOME/unexpected\"");
    f.hook("20-typo@labtop", ":");
    let text = exit(&run(&mut f.up("--verbose")), 1);
    mentions(&text, &["20-typo@labtop", "laptop", "server"]);
    assert!(!f.world.home().join("unexpected").exists());
}

#[test]
#[ignore = "P-04"]
fn unchanged_deferred_hook_is_visible_without_failing_the_update() {
    let f = UpdateFixture::new();
    f.hook("10-Later", "exit 3");
    f.hook("20-Skip-alone", "touch \"$HOME/skipped\"; exit 4");
    let text = exit(&run(&mut f.up("--plain")), 0);
    mentions(&text, &["Later", "deferred"]);
    assert!(f.world.home().join("skipped").is_file());
}

#[test]
#[ignore = "P-04"]
fn unchanged_failed_tool_install_reaches_hooks_and_skips_upgrade() {
    let f = UpdateFixture::new();
    f.hook(
        "10-tools",
        "printf '%s %s' \"$DOTS_TOOLS_OK\" \"$DOTS_TOOLS_UPGRADED\" > \"$HOME/flags\"",
    );
    let text = exit(
        &run(f.up("--verbose").env("FIXTURE_INSTALL_STATUS", "9")),
        1,
    );
    mentions(&text, &["mise tools", "exit 9"]);
    assert_eq!(f.read_home("flags"), "0 0");
    assert!(
        !f.read_home("mise-calls")
            .lines()
            .any(|line| line.starts_with("upgrade"))
    );
}

// Also covers ExceptionManifestTests.test_flags_column_is_optional_and_parsed.
#[test]
#[ignore = "P-04"]
fn unchanged_update_passes_install_only_to_flagged_selected_recipes() {
    let f = UpdateFixture::new();
    for name in ["plain-app", "self-updater", "everywhere", "not-here"] {
        f.script(
            &format!("dots/exceptions/{name}"),
            r#"printf '%s:%s\n' "${0##*/}" "$*" >> "$HOME/recipes"
printf '%s\n' "$DOTS_ROOT" "$DOTS_PROFILE" "$PWD" > "$HOME/recipe-env""#,
        );
    }
    f.write(
        "dots/exceptions.tsv",
        "# profile\tname\tflags\nlaptop\tplain-app\nlaptop\tself-updater\tinstall-only,hold=brew:thing\nserver\tnot-here\n\nbase\teverywhere\n",
    );
    f.hook("10-after-recipes", "test -f \"$HOME/recipes\"");
    let text = exit(&run(&mut f.up("--verbose")), 0);
    mentions(&text, &["self-updater"]);
    assert_eq!(
        f.read_home("recipes"),
        "plain-app:\nself-updater:--install-only\neverywhere:\n"
    );
    let root = f.world.root().display();
    assert_eq!(
        f.read_home("recipe-env"),
        format!("{root}\nlaptop\n{root}\n")
    );
}

#[test]
#[ignore = "P-04"]
fn new_recipes_read_the_callers_redirected_stdin() {
    // The packaged v0.4.0 supervisor forwarded stdin only for an interactive
    // run. 1.0 also forwards redirected caller input to each recipe.
    let f = UpdateFixture::new();
    for name in ["reader", "after-reader"] {
        f.script(
            &format!("dots/exceptions/{name}"),
            r#"IFS= read -r line
printf '%s:%s\n' "${0##*/}" "$line" >> "$HOME/read-lines""#,
        );
    }
    f.write(
        "dots/exceptions.tsv",
        "laptop\treader\nlaptop\tafter-reader\n",
    );
    let input = f.world.home().join("input");
    fs::write(&input, "first-line\nsecond-line\n").unwrap();
    exit(
        &run(f
            .up("--plain")
            .stdin(Stdio::from(fs::File::open(input).unwrap()))),
        0,
    );
    assert_eq!(
        f.read_home("read-lines"),
        "reader:first-line\nafter-reader:second-line\n"
    );
}

// Covers both legacy sync-attention cases through public diagnostics and follow-on
// work. The Node backend's exit codes and typed renderer frames are gone.
#[test]
#[ignore = "P-08"]
fn unchanged_update_reports_a_sync_issue_and_continues_maintenance() {
    let f = UpdateFixture::new();
    f.write(
        "config.toml",
        &format!("{CONFIG}dots_source_repo = \"example/update-fixture\"\n"),
    );
    f.hook("10-maintenance", "touch \"$HOME/maintenance-ran\"");
    for (configured, diagnostic) in [(false, "Git repository"), (true, "example/update-fixture")] {
        if configured {
            let remote = f.world.bare_remote("origin");
            exit(
                &run(f.world.git().args(["init", "--initial-branch=dev"])),
                0,
            );
            exit(&run(f.world.git().args(["add", "."])), 0);
            exit(
                &run(f.world.git().args(["commit", "-m", "Seed fixture"])),
                0,
            );
            exit(
                &run(f
                    .world
                    .git()
                    .args(["remote", "add", "origin"])
                    .arg(format!("file://{}", remote.display()))),
                0,
            );
        }
        let text = exit(&run(&mut f.up("--plain")), 0);
        mentions(&text, &[diagnostic, "attention", "dots sync"]);
        fs::remove_file(f.world.home().join("maintenance-ran")).unwrap();
    }
}

#[test]
#[ignore = "P-04"]
fn unchanged_plain_update_shows_failed_and_deferred_hook_output() {
    // Retained Bash fallback log paths belonged to the old updater. The
    // actionable hook output belongs in ordinary up scrollback.
    let f = UpdateFixture::new();
    f.hook("10-Waits", "echo 'fixture needs a login'; exit 3");
    f.hook(
        "20-Breaks",
        "echo 'fixture step one'; echo 'fixture disk full' >&2; exit 7",
    );
    let text = exit(&run(&mut f.up("--plain")), 1);
    mentions(
        &text,
        &[
            "Waits",
            "deferred",
            "Breaks",
            "failed",
            "fixture needs a login",
            "fixture step one",
            "fixture disk full",
        ],
    );
}

#[test]
fn unchanged_add_rejects_malformed_exception_rows_before_installing() {
    let f = UpdateFixture::new();
    f.script("dots/exceptions/one", ":");
    for (row, diagnostic) in [
        (
            "laptop\tone\tinstall-only\textra",
            "invalid installer exception row",
        ),
        ("desktop\tone", "invalid installer exception row"),
        ("laptop\tone\tsometimes", "sometimes"),
        ("laptop\tone\thold=some-app", "some-app"),
        ("laptop\t../one", "invalid installer exception name"),
    ] {
        f.write("dots/exceptions.tsv", &format!("laptop\tone\n{row}\n"));
        let text = exit(
            &run(f.dots().args(["add", "--base", "brew:fixture-package"])),
            2,
        );
        mentions(&text, &["exceptions.tsv"]);
        assert!(
            text.contains(diagnostic) || text.contains("invalid installer exception row 2"),
            "wrong diagnostic for {row:?}:\n{text}"
        );
        assert_eq!(
            fs::read_to_string(f.world.root().join("config.toml")).unwrap(),
            CONFIG,
            "{row}"
        );
    }
    f.assert_no_package_install();
}

#[test]
fn unchanged_exception_validation_names_the_required_profiles_setting() {
    let f = UpdateFixture::new();
    f.write("config.toml", "[vars]\n");
    f.write("dots/exceptions.tsv", "laptop\tone\n");
    let text = exit(
        &run(f.dots().args(["add", "--base", "brew:fixture-package"])),
        2,
    );
    mentions(&text, &["dots_profiles", "[vars]", "config.toml"]);
}

#[test]
fn unchanged_add_refuses_a_held_package_even_with_a_tap_and_version() {
    let f = UpdateFixture::new();
    f.script("dots/exceptions/pinned", ":");
    f.script("dots/exceptions/cask-recipe", ":");
    f.write(
        "dots/exceptions.tsv",
        "laptop\tpinned\thold=brew:pinned-tool\nbase\tcask-recipe\tinstall-only,hold=brew-cask:thing\n",
    );
    for (request, recipe) in [
        ("brew:tap/x/pinned-tool@1", "pinned"),
        ("brew-cask:org/tap/thing@2", "cask-recipe"),
    ] {
        let text = exit(&run(f.dots().args(["add", "--base", request])), 2);
        mentions(&text, &[request, "held", recipe]);
    }
    assert_eq!(
        fs::read_to_string(f.world.root().join("config.toml")).unwrap(),
        CONFIG
    );
    f.assert_no_package_install();
}
