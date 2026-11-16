"""The app hook `dots add` writes reaches dots without a version-specific path."""
from __future__ import annotations

import importlib.util
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

MANAGER = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("dots_github", MANAGER / "lib/dots-github.py")
dots_github = importlib.util.module_from_spec(spec)
spec.loader.exec_module(dots_github)

RELEASE = {"tag_name": "v1.2.0", "assets": [{"name": "Thing-1.2.0-macos-universal.zip"}]}


class AppHookTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="dots-github-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()

    def hook(self, home: Path) -> str:
        with mock.patch.dict(os.environ, {"DOTS_HOME": str(home)}), \
                mock.patch.object(dots_github, "fetch_release", return_value=RELEASE), \
                mock.patch.object(dots_github, "download_asset"), \
                mock.patch.object(dots_github, "archive_app", return_value="Thing.app"), \
                mock.patch.object(dots_github.platform, "system", return_value="Darwin"), \
                mock.patch.object(dots_github.platform, "machine", return_value="arm64"):
            request = dots_github.resolve("https://github.com/owner/thing")
        run = request.split('postinstall={run=', 1)[1].split(',when=', 1)[0]
        return json.loads(run)

    def test_each_install_kind_gets_a_path_that_survives_upgrades(self) -> None:
        checkout = self.base / "src/dots"
        (checkout / ".git").mkdir(parents=True)
        archive = self.base / "opt/dots"
        cellar = self.base / "brew/Cellar/dots/1.0"
        installs = self.base / "data/installs/github-ezra-cerpac-dots/1.0"
        expected = {
            checkout: f"{checkout}/bin/dots",
            archive: f"{archive}/bin/dots",
            cellar: f"{self.base}/brew/bin/dots",
            installs: '"$(command -v mise || echo "$HOME/.local/bin/mise")" x github:EzraCerpac/dots -- dots',
        }
        for home, command in expected.items():
            with self.subTest(home=home):
                self.assertEqual(self.hook(home), command + " internal github-install-app owner/thing")

    def test_homebrew_hook_runs_without_dots_on_path(self) -> None:
        prefix = self.base / "brew"
        cellar = prefix / "Cellar/dots/1.0"
        shutil.copytree(MANAGER, cellar, ignore=shutil.ignore_patterns(".git", "dots-up", "tests", "__pycache__"))
        (prefix / "bin").mkdir()
        (prefix / "bin/dots").symlink_to("../Cellar/dots/1.0/bin/dots")
        command = self.hook(cellar).replace("github-install-app owner/thing", "home")
        result = subprocess.run(["sh", "-c", command], env={"HOME": str(self.base), "PATH": "/usr/bin:/bin", "DOTS_SKILL_LINKS": "0"},
                                capture_output=True, text=True, timeout=60, check=False)
        self.assertEqual((result.returncode, result.stdout), (0, f"{cellar}\n"), result.stderr)


if __name__ == "__main__":
    unittest.main()
