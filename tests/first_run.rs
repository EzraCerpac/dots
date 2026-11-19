mod support;

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::Output;

use support::World;

fn tool_on_path(tool: &str) -> PathBuf {
    let host_path = env::var_os("PATH").expect("test runner PATH");
    env::split_paths(&host_path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(tool))
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .unwrap_or_else(|| panic!("{tool} executable required on the test runner PATH"))
        .canonicalize()
        .unwrap()
}

fn success(output: Output) -> String {
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{}\nstdout: {stdout}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    stdout
}

#[test]
fn new_first_run_needs_only_mise_and_git_on_path() {
    // This is a 1.0 guarantee. Always use the Rust binary built with this test.
    let world = World::with_dots(env!("CARGO_BIN_EXE_dots"));
    let bin = world.base().join("bin");
    fs::create_dir(&bin).unwrap();
    symlink(tool_on_path("git"), bin.join("git")).unwrap();
    // A self-update regression must only be able to replace the fixture's copy.
    fs::copy(tool_on_path("mise"), bin.join("mise")).unwrap();

    for tool in ["age", "bash", "gitleaks", "jq", "node", "python3", "uv"] {
        let result = world.command(tool).env("PATH", &bin).output();
        assert!(
            matches!(&result, Err(error) if error.kind() == ErrorKind::NotFound),
            "{tool} must be absent from the fixture PATH: {result:?}",
        );
    }

    let root = world.home().join(".config/mise");
    assert!(!root.exists());
    let dots = |args: &[&str]| {
        success(
            world
                .dots()
                .current_dir(world.home())
                .env_remove("DOTS_ROOT")
                .env("PATH", &bin)
                .env("GIT_ALLOW_PROTOCOL", "file")
                .env("MISE_TRUSTED_CONFIG_PATHS", world.base())
                .env("MISE_OFFLINE", "1")
                // The networked first-run job covers tool downloads and mise self-update.
                .env("MISE_DISABLE_TOOLS", "age,gitleaks")
                .env("MISE_SELF_UPDATE_AVAILABLE", "false")
                .args(args)
                .output()
                .unwrap(),
        )
    };
    let git = |args: &[&str]| {
        success(
            world
                .git()
                .current_dir(&root)
                .env("PATH", &bin)
                .args(args)
                .output()
                .unwrap(),
        )
    };

    dots(&["init"]);
    assert_eq!(git(&["branch", "--show-current"]).trim_end(), "dev");
    let seed = git(&["rev-parse", "HEAD"]);
    assert_eq!(git(&["rev-parse", "main"]), seed);
    assert!(git(&["remote"]).is_empty());

    let update = dots(&["up"]);
    assert!(
        update
            .lines()
            .any(|line| line == "Sync configuration (no remote configured): skipped"),
        "{update}"
    );
    assert!(
        update
            .lines()
            .any(|line| line == "Post-update settings checkpoint: skipped"),
        "{update}"
    );
    assert!(!update.contains("Needs attention"), "{update}");
    assert_eq!(git(&["rev-parse", "HEAD"]), seed);
    assert!(git(&["status", "--porcelain"]).is_empty());
}
