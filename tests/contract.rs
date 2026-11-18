mod support;

use std::fs;
use support::World;

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
