mod support;

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use support::{FixtureCommand, World, stage_executable};

// install.tsv counts this Cargo target only. Nine legacy scenarios are tested
// here, three are covered by two r01_config tests, and eight are in the drop list.
// The two referenced tests are unchanged_codex_only_gets_the_shared_skill and
// unchanged_skill_link_updates_legacy_ownership_but_leaves_user_replacements
// (the latter covers both the dangling and the user-repointed link cases).
// r01_config also covers Claude containment, installer layouts for 1.0, and
// the new case where an absent CODEX_HOME overlaps the shared skills directory.
const CLAUDE_LINK: &str = ".claude/skills/dots";
const SHARED_LINK: &str = ".agents/skills/dots";

fn agent_links(world: &World) -> [PathBuf; 2] {
    for agent in [".claude", ".agents"] {
        fs::create_dir(world.home().join(agent)).unwrap();
    }
    [CLAUDE_LINK, SHARED_LINK].map(|link| world.home().join(link))
}

fn with_skill_links(world: &World) -> FixtureCommand<'_> {
    let mut command = world.dots();
    command.env_remove("DOTS_SKILL_LINKS");
    command
}

fn refresh(world: &World) -> Output {
    let output = with_skill_links(world).arg("--version").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    output
}

fn assert_absent(path: &Path) {
    assert_eq!(
        path.symlink_metadata().unwrap_err().kind(),
        ErrorKind::NotFound,
        "{path:?}"
    );
}

fn skill_target(link: &Path) -> PathBuf {
    let target = fs::read_link(link).unwrap();
    let text = fs::read_to_string(link.join("SKILL.md")).unwrap();
    assert!(text.lines().any(|line| line == "name: dots"), "{text}");
    target
}

#[test]
fn unchanged_skill_link_opt_out() {
    let world = World::new();
    let links = agent_links(&world);

    let output = world.dots().arg("--version").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    for link in &links {
        assert_absent(link);
    }
    assert_absent(&world.home().join(".local/state/dots"));

    refresh(&world);
    for link in links {
        skill_target(&link);
    }
}

#[test]
fn unchanged_first_run_links_both_agents_to_the_skill() {
    let world = World::new();
    let [claude, shared] = agent_links(&world);

    let output = refresh(&world);
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(skill_target(&claude), skill_target(&shared));
}

#[test]
fn unchanged_second_run_keeps_existing_skill_links() {
    let world = World::new();
    let links = agent_links(&world);
    let identity = || {
        links.each_ref().map(|link| {
            let metadata = link.symlink_metadata().unwrap();
            (metadata.ino(), metadata.mtime(), metadata.mtime_nsec())
        })
    };

    refresh(&world);
    let before = identity();
    let output = refresh(&world);
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(identity(), before);
}

#[test]
fn unchanged_unrecorded_link_to_another_dots_tree_is_left_alone() {
    let world = World::new();
    let target = world.root().join("development/skills/dots");
    fs::create_dir_all(&target).unwrap();
    fs::write(
        target.join("SKILL.md"),
        "---\nname: dots\n---\nprivate copy\n",
    )
    .unwrap();
    let link = world.home().join(CLAUDE_LINK);
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(&target, &link).unwrap();

    let output = refresh(&world);
    assert_left_once(&output, &link);
    assert_eq!(fs::read_link(link).unwrap(), target);
}

fn assert_left_once(output: &Output, link: &Path) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let notice = format!("left {} ", link.display());
    assert_eq!(stderr.matches(&notice).count(), 1, "{stderr}");
}

#[test]
fn unchanged_user_directory_and_file_are_preserved_with_one_notice_each() {
    let world = World::new();
    let [claude, shared] = agent_links(&world);
    fs::create_dir(world.home().join(".codex")).unwrap();
    fs::create_dir_all(&claude).unwrap();
    fs::write(claude.join("SKILL.md"), "my own skill\n").unwrap();
    fs::create_dir(shared.parent().unwrap()).unwrap();
    fs::write(&shared, "a file\n").unwrap();

    let output = refresh(&world);
    assert_left_once(&output, &claude);
    assert_left_once(&output, &shared);
    let second = refresh(&world);
    assert!(second.stderr.is_empty(), "{second:?}");
    assert_eq!(
        fs::read(claude.join("SKILL.md")).unwrap(),
        b"my own skill\n"
    );
    assert_eq!(fs::read(shared).unwrap(), b"a file\n");
}

#[test]
fn unchanged_claude_link_to_a_configured_shared_skill_is_preserved() {
    let world = World::new();
    let [claude, shared] = agent_links(&world);
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("SKILL.md"), "deployed by my configuration\n").unwrap();
    fs::create_dir(claude.parent().unwrap()).unwrap();
    symlink(&shared, &claude).unwrap();

    refresh(&world);
    assert_eq!(fs::read_link(claude).unwrap(), shared);
    assert_eq!(
        fs::read(shared.join("SKILL.md")).unwrap(),
        b"deployed by my configuration\n"
    );
}

#[test]
fn unchanged_shared_skills_directory_cannot_escape_its_agent_directory() {
    // r01_config covers the separate Claude containment check.
    let world = World::new();
    let agents = world.home().join(".agents");
    fs::create_dir(&agents).unwrap();
    let outside = world.root().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, agents.join("skills")).unwrap();

    let output = refresh(&world);
    assert_left_once(&output, &world.home().join(SHARED_LINK));
    assert!(fs::read_dir(outside).unwrap().next().is_none());
}

#[test]
fn unchanged_explicit_claude_config_directory_can_be_outside_home() {
    let world = World::new();
    let config = world.root().join("claude-config");
    fs::create_dir(&config).unwrap();

    let output = with_skill_links(&world)
        .env("CLAUDE_CONFIG_DIR", &config)
        .arg("--version")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    skill_target(&config.join("skills/dots"));
    assert_absent(&world.home().join(".claude"));
}

#[test]
fn unchanged_no_agent_directories_means_no_skill_links() {
    let world = World::new();
    refresh(&world);
    // Rust may persist the embedded skill even when no agent root is present.
    for agent in [".claude", ".agents", ".codex"] {
        assert_absent(&world.home().join(agent));
    }
}

#[test]
#[ignore = "R-01 completion"]
fn unchanged_loading_bash_completion_and_completing_a_word_leave_skills_alone() {
    let world = World::new();
    let links = agent_links(&world);
    let dots = with_skill_links(&world);
    let bin = world.root().with_file_name("bin");
    fs::create_dir(&bin).unwrap();
    symlink(dots.get_program(), bin.join("dots")).unwrap();
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let output = world
        .command("/bin/bash")
        .env_remove("DOTS_SKILL_LINKS")
        .env("PATH", path)
        .current_dir(world.root())
        .args([
            "--noprofile",
            "--norc",
            "-c",
            r#"
loader=$(dots completions bash) || exit
eval "$loader" || exit
COMP_LINE='dots re'
COMP_POINT=${#COMP_LINE}
COMP_WORDS=(dots re)
COMP_CWORD=1
read -ra registration <<< "$(complete -p dots)"
handler=${registration[${#registration[@]}-2]}
"$handler" dots re dots
printf '%s\n' "${COMPREPLY[@]}"
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for candidate in ["remove", "reports"] {
        assert!(stdout.lines().any(|line| line == candidate), "{stdout}");
    }
    assert_absent(&world.home().join(".local/state/dots"));
    for link in links {
        assert_absent(link.parent().unwrap());
    }
}

#[test]
fn new_private_skill_survives_removal_of_the_installation() {
    for layout in [
        "mise/installs/github-ezracerpac-dots/1.0/bin/dots",
        "brew/Cellar/dots/1.0/bin/dots",
    ] {
        let world = World::new();
        let links = agent_links(&world);
        let installation = world.root().with_file_name("installation");
        let binary = installation.join(layout);
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        stage_executable(world.dots().get_program(), &binary).unwrap();

        let output = world
            .command(binary)
            .env_remove("DOTS_SKILL_LINKS")
            .current_dir(world.root())
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        fs::remove_dir_all(installation).unwrap();

        let state = world.home().join(".local/state/dots");
        let skill = state.join("skills/dots/SKILL.md");
        for private in [&state, &skill] {
            assert_eq!(private.metadata().unwrap().mode() & 0o077, 0, "{private:?}");
        }
        let embedded = include_bytes!("../skills/dots/SKILL.md");
        for link in links {
            assert_eq!(fs::read_link(&link).unwrap(), skill.parent().unwrap());
            assert_eq!(fs::read(link.join("SKILL.md")).unwrap(), embedded);
        }
    }
}

#[test]
#[ignore = "P-02"]
fn new_status_shows_agent_skill_links_without_mise() {
    let world = World::new();
    let links = agent_links(&world);
    let output = with_skill_links(&world)
        .env("PATH", world.home().join(".local/bin"))
        .arg("status")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    for link in links {
        let target = skill_target(&link);
        assert!(
            stdout.lines().any(|line| {
                line.contains(link.to_str().unwrap()) && line.contains(target.to_str().unwrap())
            }),
            "{stdout}"
        );
    }
}
