mod support;

use support::World;

#[test]
fn unchanged_help_without_configuration() {
    let world = World::new();
    let output = world
        .dots()
        .current_dir(world.home())
        .env_remove("DOTS_ROOT")
        .arg("help")
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.split_whitespace().any(|word| word == "--version"),
        "{stdout}"
    );
}
