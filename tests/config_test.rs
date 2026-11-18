mod support;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;

use support::World;

fn setup(script: &str, mode: u32) -> World {
    let world = World::new();
    fs::write(
        world.root().join("config.toml"),
        "[vars]\ndots_profiles='laptop'\n",
    )
    .unwrap();
    fs::write(
        world.root().join("config.laptop.toml"),
        "[env]\nSYNTHETIC_PROFILE_VALUE='selected'\n",
    )
    .unwrap();
    let tests = world.root().join("tests");
    fs::create_dir(&tests).unwrap();
    let script_path = tests.join("run");
    fs::write(&script_path, script).unwrap();
    fs::set_permissions(script_path, fs::Permissions::from_mode(mode)).unwrap();
    world
}

#[test]
fn new_test_help_and_os_args_reach_the_executable_with_native_streams() {
    let world = setup(
        r#"#!/bin/sh
read -r input
printf '%s\000' "$@" "$input" "$DOTS_PROFILE" "$SYNTHETIC_PROFILE_VALUE" "$DOTS_ROOT" "$PWD"
printf 'stderr\n' >&2
exit 23
"#,
        0o700,
    );
    let mut child = world
        .dots()
        .env("SETUP_PROFILE", "laptop")
        .env("MISE_TRUSTED_CONFIG_PATHS", world.root())
        .args(["test", "--help", "--", "two words", ""])
        .arg(OsString::from_vec(vec![0xff]))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"caller input\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(23), "{output:?}");
    let root = world.root().as_os_str().as_encoded_bytes();
    assert_eq!(
        output.stdout.split(|b| *b == 0).collect::<Vec<_>>(),
        [
            b"--help".as_slice(),
            b"--",
            b"two words",
            b"",
            b"\xff",
            b"caller input",
            b"laptop",
            b"selected",
            root,
            root,
            b"",
        ],
    );
    assert_eq!(output.stderr, b"stderr\n");
    let reports = world.home().join(".local/state/dots/reports");
    let files = fs::read_dir(reports)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(files.len(), 1);
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(files[0].path()).unwrap()).unwrap();
    assert_eq!(report["command"], "test");
    assert_eq!(report["status"], 23);
    assert_eq!(report["stages"][0]["outcome"], "failed");
    assert!(report["next"].as_str().unwrap().contains("dots test"));
    assert!(!report.to_string().contains("caller input"));
}

#[test]
fn new_test_refuses_to_interpret_a_nonexecutable_script() {
    let world = setup("#!/bin/sh\nprintf 'unexpected\n'\n", 0o600);
    let output = world.dots().arg("test").output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tests/run") && stderr.contains("not executable"),
        "{output:?}"
    );
}
