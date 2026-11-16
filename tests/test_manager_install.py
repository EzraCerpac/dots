"""The installed manager: its version, how it was installed, and the agent skill links.

Each test lays out fake mise and Homebrew installations of two versions in a
temporary directory and runs them with a private HOME. Nothing touches the
real machine.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

MANAGER = Path(__file__).resolve().parents[1]
IGNORE = shutil.ignore_patterns("dots-up", "tests", "packaging", ".git", ".github", "__pycache__")


def install_tree(home: Path, version: str) -> Path:
    """A release tree of VERSION whose skill names its version."""
    shutil.copytree(MANAGER, home, ignore=IGNORE)
    (home / "VERSION").write_text(f"{version}\n")
    skill = home / "skills/dots/SKILL.md"
    skill.write_text(skill.read_text() + f"\nRelease marker: {version}\n")
    return home


class ManagerInstallTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="dots-install-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.home = self.base / "home"
        self.home.mkdir()
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("DOTS_", "MISE_", "XDG_", "CLAUDE_", "CODEX_"))}
        self.env.update(HOME=str(self.home), DOTS_ROOT=str(self.base / "no-config"))

    # Layouts -----------------------------------------------------------------

    def mise_install(self, version: str) -> Path:
        """mise's github backend: one directory per version, bin/ on PATH."""
        return install_tree(self.base / "mise/installs/github-ezracerpac-dots" / version, version)

    def brew_install(self, version: str) -> Path:
        """A Homebrew keg: libexec holds the tree; opt and bin link to the keg."""
        prefix = self.base / "brew"
        keg = prefix / "Cellar/dots" / version
        install_tree(keg / "libexec", version)
        (keg / "bin").mkdir()
        (keg / "bin/dots").symlink_to("../libexec/bin/dots")
        for link, target in ((prefix / "opt/dots", f"../Cellar/dots/{version}"),
                             (prefix / "bin/dots", f"../Cellar/dots/{version}/bin/dots")):
            link.parent.mkdir(parents=True, exist_ok=True)
            if link.is_symlink():
                link.unlink()
            link.symlink_to(target)
        return keg

    def brew_upgrade(self, old: str, new: str) -> None:
        """brew upgrade with its default install cleanup: relink, then delete the old keg."""
        self.brew_install(new)
        shutil.rmtree(self.base / "brew/Cellar/dots" / old)

    def agents(self, *names: str) -> None:
        for name in names:
            (self.home / name).mkdir()

    def dots(self, executable: Path, *args: str, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([str(executable), *args], env={**self.env, **env}, cwd=self.base,
                              capture_output=True, text=True, timeout=30, check=False)

    @property
    def claude_link(self) -> Path:
        return self.home / ".claude/skills/dots"

    @property
    def agents_link(self) -> Path:
        return self.home / ".agents/skills/dots"

    # Version -----------------------------------------------------------------

    def test_version_names_the_release_and_installer(self) -> None:
        mise = self.mise_install("0.3.0")
        self.assertEqual(self.dots(mise / "bin/dots", "--version").stdout, "dots 0.3.0 (mise)\n")
        self.brew_install("0.3.0")
        self.assertEqual(self.dots(self.base / "brew/bin/dots", "--version").stdout, "dots 0.3.0 (homebrew)\n")
        archive = install_tree(self.base / "unpacked", "0.3.0")
        self.assertEqual(self.dots(archive / "bin/dots", "--version").stdout, "dots 0.3.0 (archive)\n")

    def test_a_checkout_reports_git_describe_or_dev(self) -> None:
        checkout = self.base / "checkout"
        shutil.copytree(MANAGER, checkout, ignore=IGNORE)
        (checkout / ".git").mkdir()  # not a valid repository: describe fails
        self.assertEqual(self.dots(checkout / "bin/dots", "--version").stdout, "dots dev (checkout)\n")

    # Skill links -------------------------------------------------------------

    def test_first_run_links_the_skill_for_agents_in_use(self) -> None:
        self.agents(".claude", ".agents")
        mise = self.mise_install("0.3.0")
        result = self.dots(mise / "bin/dots", "--version")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")
        for link in (self.claude_link, self.agents_link):
            self.assertEqual(os.readlink(link), str(mise / "skills/dots"))
            self.assertIn("Release marker: 0.3.0", (link / "SKILL.md").read_text())

    def test_codex_alone_gets_the_shared_agents_directory(self) -> None:
        self.agents(".codex")
        mise = self.mise_install("0.3.0")
        self.dots(mise / "bin/dots", "--version")
        self.assertEqual(os.readlink(self.agents_link), str(mise / "skills/dots"))
        self.assertFalse((self.home / ".claude").exists())

    def test_a_second_run_changes_nothing(self) -> None:
        self.agents(".claude", ".agents")
        mise = self.mise_install("0.3.0")
        self.dots(mise / "bin/dots", "--version")
        before = os.lstat(self.claude_link)
        result = self.dots(mise / "bin/dots", "--version")
        self.assertEqual(result.stderr, "")
        after = os.lstat(self.claude_link)
        self.assertEqual((before.st_ino, before.st_mtime_ns), (after.st_ino, after.st_mtime_ns))

    def test_a_mise_upgrade_moves_the_links_to_the_new_version(self) -> None:
        self.agents(".claude", ".agents")
        old = self.mise_install("0.3.0")
        self.dots(old / "bin/dots", "--version")
        new = self.mise_install("0.4.0")
        self.dots(new / "bin/dots", "--version")
        for link in (self.claude_link, self.agents_link):
            self.assertEqual(os.readlink(link), str(new / "skills/dots"))
            self.assertIn("Release marker: 0.4.0", (link / "SKILL.md").read_text())
        # mise pruning the old version later leaves nothing dangling.
        shutil.rmtree(old)
        self.assertTrue((self.claude_link / "SKILL.md").is_file())

    def test_a_brew_upgrade_keeps_the_links_working_through_opt(self) -> None:
        self.agents(".claude", ".agents")
        self.brew_install("0.3.0")
        self.dots(self.base / "brew/bin/dots", "--version")
        opt_skill = str(self.base / "brew/opt/dots/libexec/skills/dots")
        self.assertEqual(os.readlink(self.claude_link), opt_skill)
        self.brew_upgrade("0.3.0", "0.4.0")
        # Before dots runs again, the agents already read the new release.
        for link in (self.claude_link, self.agents_link):
            self.assertIn("Release marker: 0.4.0", (link / "SKILL.md").read_text())
        self.dots(self.base / "brew/bin/dots", "--version")
        self.assertEqual(os.readlink(self.agents_link), opt_skill)

    def test_a_dangling_link_from_a_removed_install_is_replaced(self) -> None:
        self.agents(".claude")
        old = self.mise_install("0.3.0")
        self.dots(old / "bin/dots", "--version")
        shutil.rmtree(old)  # mise prune or uninstall
        new = self.mise_install("0.4.0")
        self.dots(new / "bin/dots", "--version")
        self.assertEqual(os.readlink(self.claude_link), str(new / "skills/dots"))

    def test_a_link_the_user_repointed_is_left_alone(self) -> None:
        self.agents(".claude")
        mise = self.mise_install("0.3.0")
        self.dots(mise / "bin/dots", "--version")
        own = self.base / "own-skill"
        own.mkdir()
        self.claude_link.unlink()
        self.claude_link.symlink_to(own)
        new = self.mise_install("0.4.0")
        self.dots(new / "bin/dots", "--version")
        self.assertEqual(os.readlink(self.claude_link), str(own))

    def test_a_link_the_user_made_into_another_dots_tree_is_left_alone(self) -> None:
        self.agents(".claude")
        development = install_tree(self.base / "development", "9.9.9")
        self.claude_link.parent.mkdir(parents=True)
        self.claude_link.symlink_to(development / "skills/dots")
        mise = self.mise_install("0.3.0")
        result = self.dots(mise / "bin/dots", "--version")
        self.assertIn("left ", result.stderr)
        self.assertEqual(os.readlink(self.claude_link), str(development / "skills/dots"))

    def test_what_the_user_owns_is_left_alone_with_one_notice(self) -> None:
        self.agents(".claude", ".agents", ".codex")
        own_directory = self.claude_link
        (own_directory / "SKILL.md").parent.mkdir(parents=True)
        (own_directory / "SKILL.md").write_text("my own skill\n")
        self.agents_link.parent.mkdir(parents=True)
        self.agents_link.write_text("a file\n")
        mise = self.mise_install("0.3.0")
        first = self.dots(mise / "bin/dots", "--version")
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertEqual(first.stderr.count("left "), 2, first.stderr)
        second = self.dots(mise / "bin/dots", "--version")
        self.assertEqual(second.stderr, "")
        self.assertEqual((own_directory / "SKILL.md").read_text(), "my own skill\n")
        self.assertEqual(self.agents_link.read_text(), "a file\n")

    def test_a_foreign_link_is_left_alone(self) -> None:
        # A configuration that deploys the skill itself: Claude links to the
        # shared copy, which is no dots installation.
        self.agents(".claude", ".agents")
        shared = self.home / ".agents/skills/dots"
        shared.mkdir(parents=True)
        (shared / "SKILL.md").write_text("deployed by my configuration\n")
        self.claude_link.parent.mkdir(parents=True)
        self.claude_link.symlink_to(shared)
        mise = self.mise_install("0.3.0")
        self.dots(mise / "bin/dots", "--version")
        self.assertEqual(os.readlink(self.claude_link), str(shared))
        self.assertEqual((shared / "SKILL.md").read_text(), "deployed by my configuration\n")

    def test_a_skills_directory_linked_outside_the_agent_directory_is_not_written(self) -> None:
        self.agents(".claude", ".agents")
        elsewhere = self.base / "elsewhere"
        elsewhere.mkdir()
        (self.home / ".claude/skills").symlink_to(elsewhere)
        (self.home / ".agents/skills").symlink_to(elsewhere)
        mise = self.mise_install("0.3.0")
        result = self.dots(mise / "bin/dots", "--version")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(elsewhere.iterdir()), [])
        self.assertEqual(result.stderr.count("left "), 2, result.stderr)

    def test_an_explicit_claude_config_dir_is_taken_as_given(self) -> None:
        config = self.base / "claude-config"  # outside HOME
        config.mkdir()
        mise = self.mise_install("0.3.0")
        self.dots(mise / "bin/dots", "--version", CLAUDE_CONFIG_DIR=str(config))
        self.assertEqual(os.readlink(config / "skills/dots"), str(mise / "skills/dots"))

    def link_step(self, home: Path, link: Path) -> None:
        """Run only the link step, as dots_skill_refresh does after its checks."""
        script = f'source "{home}/lib/dots-manager.sh"; dots_skill_link "{link}" "{home}/skills/dots"'
        subprocess.run(["bash", "-c", script], env=self.env, check=True, timeout=30)

    def test_a_link_created_after_the_absence_check_is_not_clobbered(self) -> None:
        # Between dots seeing no link and creating one, the user makes theirs.
        self.agents(".claude")
        mise = self.mise_install("0.3.0")
        own = self.base / "own-skill"
        own.mkdir()
        self.claude_link.parent.mkdir(parents=True)
        self.claude_link.symlink_to(own)
        self.link_step(mise, self.claude_link)
        self.assertEqual(os.readlink(self.claude_link), str(own))
        self.dots(self.mise_install("0.4.0") / "bin/dots", "--version")
        self.assertEqual(os.readlink(self.claude_link), str(own))

    def test_a_link_repointed_after_the_ownership_check_is_not_clobbered(self) -> None:
        # Between dots confirming its own link and replacing it, the user repoints it.
        self.agents(".claude")
        old = self.mise_install("0.3.0")
        self.dots(old / "bin/dots", "--version")
        own = self.base / "own-skill"
        own.mkdir()
        self.claude_link.unlink()
        self.claude_link.symlink_to(own)
        self.link_step(self.mise_install("0.4.0"), self.claude_link)
        self.assertEqual(os.readlink(self.claude_link), str(own))

    def test_opt_out(self) -> None:
        self.agents(".claude", ".agents")
        mise = self.mise_install("0.3.0")
        self.dots(mise / "bin/dots", "--version", DOTS_SKILL_LINKS="0")
        self.assertFalse(self.claude_link.is_symlink() or self.claude_link.exists())
        self.assertFalse(self.agents_link.is_symlink() or self.agents_link.exists())

    def test_no_agent_directories_no_links(self) -> None:
        mise = self.mise_install("0.3.0")
        result = self.dots(mise / "bin/dots", "--version")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(sorted(path.name for path in self.home.iterdir()), [])

    def test_a_development_home_leaves_the_links_alone(self) -> None:
        self.agents(".claude")
        mise = self.mise_install("0.3.0")
        self.dots(mise / "bin/dots", "--version")
        development = install_tree(self.base / "development", "9.9.9")
        self.dots(mise / "bin/dots", "--version", DOTS_HOME=str(development))
        self.assertEqual(os.readlink(self.claude_link), str(mise / "skills/dots"))

    def test_status_lines_name_each_link(self) -> None:
        self.agents(".claude")
        mise = self.mise_install("0.3.0")
        self.dots(mise / "bin/dots", "--version")
        script = f'source "{mise}/lib/dots-manager.sh"; dots_skill_status "{mise}"'
        result = subprocess.run(["bash", "-c", script], env=self.env, capture_output=True, text=True, check=False)
        self.assertEqual(result.stdout, f"Agent skill (claude): {self.claude_link} -> {mise}/skills/dots\n")


if __name__ == "__main__":
    unittest.main()
