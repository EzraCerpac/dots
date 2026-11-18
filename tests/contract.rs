mod support;

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Output;
use support::{FixtureCommand, World};
use toml_edit::DocumentMut;

fn run(mut command: FixtureCommand<'_>, args: &[impl AsRef<OsStr>]) -> Output {
    command.args(args).output().unwrap()
}

fn success(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn usage_error(output: Output) -> String {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    stderr
}

fn isolated_mise(world: &World) -> FixtureCommand<'_> {
    let mut dots = world.dots();
    dots.env("MISE_SYSTEM_CONFIG_DIR", world.home().join("mise-system"))
        .env("MISE_CEILING_PATHS", world.root().parent().unwrap())
        .env("MISE_TRUSTED_CONFIG_PATHS", world.root().parent().unwrap())
        .env("MISE_OFFLINE", "1")
        .env("MISE_NO_HOOKS", "1")
        .env("MISE_AUTO_INSTALL", "0")
        .env("MISE_EXEC_AUTO_INSTALL", "0");
    dots
}

#[test]
fn unchanged_missing_or_non_executable_command_is_unknown() {
    let world = World::new();
    let missing = world.root().join("dots/commands/nope");
    let output = world.dots().arg("nope").output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        output.stderr,
        b"dots: unknown command 'nope'; use dots help\n"
    );
    assert!(output.stdout.is_empty(), "{output:?}");

    fs::write(missing, "#!/bin/sh\necho ran\n").unwrap();
    let output = world.dots().arg("nope").output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        output.stderr,
        b"dots: unknown command 'nope'; use dots help\n"
    );
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[test]
fn unchanged_command_names_cannot_contain_paths() {
    let world = World::new();
    for name in ["dots/commands/hello", "../config/dots/commands/hello"] {
        let output = world.dots().arg(name).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(stderr.lines().count(), 1, "{stderr}");
        assert!(stderr.contains("use dots help"), "{stderr}");
    }
}

#[test]
fn new_root_respects_configuration_precedence_from_elsewhere() {
    let world = World::new();
    let xdg = world.home().join("custom-config");
    let xdg_root = xdg.join("mise");
    let home_root = world.home().join(".config/mise");
    let mise_root = world.home().join("mise-config-dir");
    for root in [&xdg_root, &home_root, &mise_root] {
        fs::create_dir_all(root).unwrap();
        fs::copy(world.root().join("config.toml"), root.join("config.toml")).unwrap();
    }

    let mut dots = world.dots();
    dots.allow_config_fallback()
        .current_dir(world.home())
        .arg("root")
        .env("XDG_CONFIG_HOME", xdg)
        .env("MISE_CONFIG_DIR", &mise_root);
    for (expected, remove_before_next) in [
        (world.root(), "DOTS_ROOT"),
        (&mise_root, "MISE_CONFIG_DIR"),
        (&xdg_root, "XDG_CONFIG_HOME"),
    ] {
        assert_eq!(
            success(dots.output().unwrap()),
            format!("{}\n", expected.display())
        );
        dots.env_remove(remove_before_next);
    }
    assert_eq!(
        success(dots.output().unwrap()),
        format!("{}\n", home_root.display())
    );
}

#[test]
fn new_profile_rejects_unlisted_selection_from_quoted_vars() {
    let world = World::new();
    fs::write(
        world.root().join("config.toml"),
        "[\"vars\"]\n\"dots_profiles\" = \"laptop server\"\n",
    )
    .unwrap();
    let stderr = usage_error(run(
        world.dots(),
        &["profile", "--profile", "desktop", "--machine-id", "fx"],
    ));
    for value in ["desktop", "laptop", "server"] {
        assert!(stderr.contains(value), "{stderr}");
    }
    assert!(!world.root().join("miserc.toml").exists());
    assert!(!world.root().join("config.local.toml").exists());
}

#[test]
fn new_profile_refuses_malformed_or_aliased_selection_files() {
    let world = World::new();
    let early = world.root().join("miserc.toml");
    let local = world.root().join("config.local.toml");
    let select = || {
        run(
            world.dots(),
            &["profile", "--profile", "server", "--machine-id", "fx"],
        )
    };

    let bad_selector = b"[\"env\"]\nSETUP_PROFILE = \"laptop\"\n";
    let local_settings = b"[env]\nSETUP_PROFILE = \"laptop\"\n";
    fs::write(&early, bad_selector).unwrap();
    fs::write(&local, local_settings).unwrap();
    let stderr = usage_error(select());
    assert!(stderr.contains("miserc.toml"), "{stderr}");
    assert_eq!(fs::read(&early).unwrap(), bad_selector);
    assert_eq!(fs::read(&local).unwrap(), local_settings);

    fs::remove_file(&local).unwrap();
    fs::write(&early, b"# Early settings\n").unwrap();
    symlink("miserc.toml", &local).unwrap();
    let stderr = usage_error(select());
    assert!(
        stderr.contains("miserc.toml") && stderr.contains("config.local.toml"),
        "{stderr}"
    );
    assert_eq!(fs::read_link(&local).unwrap(), Path::new("miserc.toml"));
    assert_eq!(fs::read(&early).unwrap(), b"# Early settings\n");
}

fn minimal_config(world: &World, source: &str) {
    fs::remove_dir_all(world.root().join("dots")).unwrap();
    fs::remove_file(world.root().join("config.laptop.toml")).unwrap();
    fs::write(world.root().join("config.toml"), source).unwrap();
}

#[test]
fn unchanged_help_and_command_lookup_work_without_dots_directory() {
    let world = World::new();
    minimal_config(&world, "[vars]\ndots_profiles = \"laptop server\"\n");
    let help = success(run(world.dots(), &["help"]));
    assert!(!help.contains("Configuration commands"), "{help}");
    let stderr = usage_error(run(world.dots(), &["anything"]));
    assert!(stderr.contains("unknown command 'anything'"), "{stderr}");
    assert!(stderr.contains("dots help"), "{stderr}");
}

#[test]
#[ignore = "P-02"]
fn new_exceptions_work_without_dots_directory() {
    let world = World::new();
    minimal_config(&world, "[vars]\ndots_profiles = \"laptop server\"\n");
    success(run(isolated_mise(&world), &["exceptions"]));
}

fn update_mise(world: &World) -> FixtureCommand<'_> {
    let fake = world.home().join("mock-mise");
    // Keep the disposable update independent of installed tools and package managers.
    // The mise self-update and tool mutations are fixture no-ops.
    fs::write(
        &fake,
        r#"#!/bin/sh
while [ "$#" -ge 2 ]; do
    case "$1" in -C|-E) shift 2 ;; *) break ;; esac
done
case "$*" in
    --version) printf '2026.11.9\n' ;;
    'bootstrap dotfiles paths --json') printf '{"entries":[]}\n' ;;
    'bootstrap dotfiles status --json'|'bootstrap packages status --json'|'ls --json') printf '{}\n' ;;
    install*|upgrade*) ;;
    self-update*) : > "${0%/*}/mise-self-update-ran" ;;
    which*) exit 1 ;;
    *) printf 'unexpected mise request: %s\n' "$*" >&2; exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let mut dots = isolated_mise(world);
    dots.env("DOTS_MISE_BIN", fake)
        .env("SETUP_PROFILE", "laptop")
        .env("SETUP_MACHINE_ID", "fx");
    dots
}

#[test]
#[ignore = "P-04"]
fn new_up_works_without_dots_directory() {
    let world = World::new();
    minimal_config(&world, "[vars]\ndots_profiles = \"laptop server\"\n");
    let stdout = success(run(update_mise(&world), &["--plain", "up"]));
    let plain = stdout.to_ascii_lowercase();
    assert!(
        plain.lines().any(|line| {
            line.contains("sync")
                && line.contains("skip")
                && (line.contains("no remote") || line.contains("local-only"))
        }),
        "{stdout}"
    );
    assert!(!plain.contains("needs attention"), "{stdout}");
    assert!(world.home().join("mise-self-update-ran").is_file());
    assert!(!world.root().join("dots").exists());
}

#[test]
#[ignore = "P-04"]
fn new_up_names_missing_profiles_setting_and_file() {
    let world = World::new();
    minimal_config(&world, "# No profiles\n");
    let output = run(update_mise(&world), &["--plain", "up"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr
            .lines()
            .any(|line| line.contains("dots_profiles") && line.contains("config.toml")),
        "{stderr}"
    );
}

fn read_toml(path: &Path) -> DocumentMut {
    fs::read_to_string(path).unwrap().parse().unwrap()
}

#[test]
#[ignore = "P-03"]
fn unchanged_init_creates_layout_and_keeps_edits_on_rerun() {
    let world = World::new();
    let dir = world.root().with_file_name("fresh config");
    let mut init = isolated_mise(&world);
    init.current_dir(dir.parent().unwrap())
        .arg("init")
        .arg(dir.file_name().unwrap());
    success(init.output().unwrap());
    assert_eq!(
        read_toml(&dir.join("config.toml"))["vars"]["dots_profiles"].as_str(),
        Some("default")
    );
    for path in [
        "hooks/update.d",
        "hooks/bootstrap.d",
        "commands",
        "exceptions",
    ] {
        assert!(dir.join("dots").join(path).is_dir(), "{path}");
    }
    let files = [
        "config.toml",
        "dots/README.md",
        "dots/exceptions.tsv",
        "dots/retired-dotfiles",
        "dots/retired-declarations",
        ".gitleaks.toml",
        ".gitignore",
    ];
    for file in files {
        assert!(dir.join(file).is_file(), "{file}");
    }

    let probe = dir.with_file_name("ignore-probe");
    fs::create_dir(&probe).unwrap();
    let mut git = world.git();
    git.current_dir(&probe).args(["init", "-q"]);
    success(git.output().unwrap());
    fs::copy(dir.join(".gitignore"), probe.join(".gitignore")).unwrap();
    for (path, expected) in [
        ("miserc.toml", 0),
        ("config.local.toml", 0),
        ("config.host-fx.toml", 0),
        ("mise.lock", 0),
        ("mise.laptop.lock", 0),
        ("config.toml", 1),
        ("dots/README.md", 1),
    ] {
        let output = world
            .git()
            .current_dir(&probe)
            .args(["check-ignore", "--no-index", "-q", path])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(expected), "{path}: {output:?}");
    }

    fs::write(dir.join("dots/README.md"), "user documentation\n").unwrap();
    let before = files.map(|file| fs::read(dir.join(file)).unwrap());
    success(init.output().unwrap());
    assert_eq!(files.map(|file| fs::read(dir.join(file)).unwrap()), before);
}

#[test]
#[ignore = "P-03"]
fn unchanged_init_edits_the_physical_config_referent() {
    let world = World::new();
    let config = world.root().join("config.toml");
    let outside = world.home().join("outside");
    let child = outside.join("child");
    fs::create_dir_all(&child).unwrap();
    let target = outside.join("settings.toml");
    fs::write(&target, "[\"vars\"] # vars header\n'dots_state_repo' = \"example/private\" # state\n[env]\ndots_profiles = \"unrelated\"\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    symlink(&child, world.root().join("hop")).unwrap();
    let decoy = world.root().join("settings.toml");
    fs::write(&decoy, b"# Logical traversal would edit this file\n").unwrap();
    fs::remove_file(&config).unwrap();
    symlink("hop/../settings.toml", &config).unwrap();

    success(run(isolated_mise(&world), &["init"]));
    assert_eq!(
        fs::read_link(&config).unwrap(),
        Path::new("hop/../settings.toml")
    );
    assert_eq!(
        fs::read(&decoy).unwrap(),
        b"# Logical traversal would edit this file\n"
    );
    assert_eq!(
        target.metadata().unwrap().permissions().mode() & 0o777,
        0o640
    );
    let text = fs::read_to_string(&target).unwrap();
    assert!(text.contains("[\"vars\"] # vars header"), "{text}");
    let doc: DocumentMut = text.parse().unwrap();
    assert_eq!(
        doc["vars"]["dots_state_repo"].as_str(),
        Some("example/private")
    );
    assert_eq!(doc["vars"]["dots_profiles"].as_str(), Some("default"));
    assert_eq!(doc["env"]["dots_profiles"].as_str(), Some("unrelated"));

    success(run(isolated_mise(&world), &["init"]));
    assert_eq!(fs::read_to_string(&target).unwrap(), text);
}

#[test]
#[ignore = "P-03"]
fn unchanged_init_keeps_an_existing_quoted_profile_setting() {
    let world = World::new();
    let config = world.root().join("config.toml");
    let original = b"[\"vars\"]\n\"dots_profiles\" = \"laptop server\"\n";
    fs::write(&config, original).unwrap();
    success(run(isolated_mise(&world), &["init"]));
    assert_eq!(fs::read(&config).unwrap(), original);
}

#[test]
#[ignore = "P-03"]
fn new_init_refuses_a_scalar_vars_setting_without_rewriting_it() {
    let world = World::new();
    let config = world.root().join("config.toml");
    let original = b"vars = \"cannot hold the required key\"\n";
    fs::write(&config, original).unwrap();
    let output = run(isolated_mise(&world), &["init"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("vars"), "{stderr}");
    assert_eq!(fs::read(&config).unwrap(), original);
}

#[test]
#[ignore = "P-03"]
fn new_init_adds_profiles_to_existing_toml_without_changing_other_bytes() {
    let world = World::new();
    for (directory, original, has_vars) in [
        (
            "with-vars",
            "# Keep this comment\n\n[vars]\nname = \"x\"\ndots_source_repo = \"a/b\" # keep\n\n[env]\nKEEP = \"yes\"\n",
            true,
        ),
        (
            "without-vars",
            "# Keep this comment\n[env]\nKEEP = \"yes\"\n",
            false,
        ),
    ] {
        let dir = world.root().with_file_name(directory);
        fs::create_dir(&dir).unwrap();
        let config = dir.join("config.toml");
        fs::write(&config, original).unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();

        success(run(
            isolated_mise(&world),
            &[OsStr::new("init"), dir.as_os_str()],
        ));
        let doc = read_toml(&config);
        assert_eq!(doc["vars"]["dots_profiles"].as_str(), Some("default"));
        assert_eq!(
            config.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = fs::read_to_string(&config).unwrap();
        // Locate an insertion between complete lines and inspect exactly what it adds.
        let added = std::iter::once(0)
            .chain(original.match_indices('\n').map(|(index, _)| index + 1))
            .find_map(|index| {
                let (before, after) = original.split_at(index);
                text.strip_prefix(before)?.strip_suffix(after)
            })
            .unwrap_or_else(|| panic!("existing bytes changed in {directory}: {text}"));
        assert!(!added.contains('#'), "unexpected added comment: {added}");
        let fragment: DocumentMut = added.parse().unwrap();
        let vars = if has_vars {
            fragment.as_table()
        } else {
            assert_eq!(fragment.as_table().len(), 1, "{added}");
            fragment["vars"].as_table().unwrap()
        };
        assert_eq!(vars.len(), 1, "{added}");
        assert_eq!(vars["dots_profiles"].as_str(), Some("default"), "{added}");
    }
}
