mod support;

use std::fs;
use std::io::ErrorKind;
use support::World;

#[test]
#[ignore = "R-01 config"]
fn unchanged_skill_link_opt_out() {
    let world = World::new();
    for agent in [".claude", ".agents"] {
        fs::create_dir(world.home().join(agent)).unwrap();
    }

    let output = world.dots().arg("--version").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let links = [".claude/skills/dots", ".agents/skills/dots"].map(|link| world.home().join(link));
    for link in &links {
        assert_eq!(
            link.symlink_metadata().unwrap_err().kind(),
            ErrorKind::NotFound,
            "{link:?}"
        );
    }

    let output = world
        .dots()
        .env_remove("DOTS_SKILL_LINKS")
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    for link in &links {
        assert!(
            link.is_symlink() && link.join("SKILL.md").is_file(),
            "{link:?}"
        );
    }
}
