"""Package maintenance through dots, with real mise config and local Git sync.

The fixtures simulate machine package commands. Each host uses a separate
package inventory, mise installation, and checkout of a local Git remote.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

from lib.prereq import mise_binary, requires

MANAGER = Path(__file__).resolve().parents[1]

BASE = """
[vars]
dots_profiles = "desktop server"
dots_source_repo = "fixture/packages"
dots_self_updating_casks = "manually-updated"
"""

# Let real mise select and filter the declarations with every package manager
# disabled. Only live package operations use the synthetic inventory.
PACKAGE_BACKEND = r"""
import json
import os
import subprocess
import sys
from pathlib import Path

real = os.environ["FIXTURE_MISE"]
args = sys.argv[1:]
index = 0
while args[index:index+1] in (["-C"], ["-E"]):
    index += 2
prefix, command = args[:index], args[index:]
if command == ["--version"] and os.environ.get("FIXTURE_MISE_PLATFORM"):
    print("2026.11.9 " + os.environ["FIXTURE_MISE_PLATFORM"])
    sys.exit(0)
if command == ["bootstrap", "packages", "where", "brew:mas"]:
    mas_prefix = os.environ.get("FIXTURE_BREW_MAS_PREFIX")
    if not mas_prefix or not (Path(os.environ["HOME"]) / "installed/brew:mas").exists():
        sys.exit(1)
    print(mas_prefix)
    sys.exit(0)
if command[:2] != ["bootstrap", "packages"] or command[2] == "use":
    os.execv(real, [real, *args])

operation, *options = command[2:]
manager = options[options.index("--manager") + 1] if "--manager" in options else None
if os.environ.get("FIXTURE_NATIVE_MAS") == "1" and (operation == "status" or manager == "mas"):
    os.execv(real, [real, *args])
requested = [item for item in options if not item.startswith("-") and ":" in item]
allowlist = os.environ.get("MISE_SYSTEM_PACKAGES_MANAGERS")
declarations = subprocess.check_output(
    [real, *prefix, "bootstrap", "packages", "status", "--json"],
    env={**os.environ, "MISE_SYSTEM_PACKAGES_MANAGERS": "", "MISE_QUIET": "1"},
)
if operation == "status" and allowlist == "":
    sys.stdout.buffer.write(declarations)
    sys.exit(0)

status = json.loads(declarations)
enabled = {manager} if manager else (set(status) if allowlist is None else set(allowlist.split(",")))
packages = {
    f"{name}:{row['package']}"
    for name, state in status.items() if name in enabled
    for row in state["packages"]
}
if requested:
    packages = set(requested)
log = Path(os.environ["HOME"]) / "packages.log"
with log.open("a") as stream:
    print(json.dumps(command[2:]), file=stream)
    if "mas" in enabled and "mas" in status and manager is None:
        print(json.dumps(["mas-probe"]), file=stream)

inventory = Path(os.environ["HOME"]) / "installed"
inventory.mkdir(exist_ok=True)
if operation == "apply":
    if "--dry-run" in options:
        sys.exit(0)
    for key in sorted(packages):
        if key.endswith(":fixture-broken"):
            sys.exit("fixture package installer failed")
        if not (inventory / key).exists():
            (inventory / key).touch()
elif operation == "upgrade":
    pass
elif operation == "status":
    for name, state in status.items():
        if name not in enabled:
            continue
        state.update(available=True, reason="")
        for row in state["packages"]:
            key = f"{name}:{row['package']}"
            if (inventory / key).exists():
                row.update(state="installed", installed_version="1")
            else:
                row.update(state="missing")
            if row["package"] == "auto-updated":
                row["auto_updates"] = True
    print(json.dumps(status))
else:
    sys.exit(f"unexpected fixture package command: {command}")
"""


@requires("mise", "git", "node", "jq", "uv", "gitleaks")
class PackageTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="dots-packages-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.executable("mise", f"#!{sys.executable}\n{PACKAGE_BACKEND}")
        self.executable("uname", """#!/bin/sh
case "$1:$FIXTURE_OS" in
    -s:macos) echo Darwin ;;
    -s:linux) echo Linux ;;
    -m:macos) echo "${FIXTURE_ARCH:-$(/usr/bin/uname -m)}" ;;
    *) exec /usr/bin/uname "$@" ;;
esac
""")
        self.executable("apt-get", "#!/bin/sh\nexit 99\n")
        self.executable("brew", "#!/bin/sh\nexit 99\n")
        self.executable("mas", """#!/bin/sh
printf '%s\n' "$*" >> "$HOME/mas.log"
case "$*" in
    'list --json')
        if [ -e "$HOME/mas-installed" ]; then
            printf '[{"adamID":123,"name":"Fixture App","version":"1"}]\n'
        else
            printf '[{"adamID":999,"name":"Unrelated Fixture App","version":"1"}]\n'
        fi ;;
    'install 123') touch "$HOME/mas-installed" ;;
    'upgrade 123') ;;
    *) exit 99 ;;
esac
""")
        self.executable("sudo", """#!/bin/sh
[ "$*" = "-n true" ] && [ "${FIXTURE_SUDO:-0}" = 1 ]
""")
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("DOTS_", "MISE_", "SETUP_", "XDG_", "GIT_", "__MISE_"))}
        self.env.update(
            PATH=f"{self.bin}:{self.env['PATH']}", FIXTURE_MISE=mise_binary(),
            DOTS_MISE_BIN=str(self.bin / "mise"), DOTS_UP_BIN="/nonexistent/dots-up", DOTS_SKILL_LINKS="0",
            SETUP_PROFILE="desktop", SETUP_MACHINE_ID="fixture", SETUP_MISE_BIN=str(self.bin / "mise"),
            MISE_SELF_UPDATE_AVAILABLE="0", TMPDIR=str(self.root),
            MISE_SYSTEM_CONFIG_DIR=str(self.root / "system"), MISE_TRUSTED_CONFIG_PATHS=str(self.root),
            MISE_CEILING_PATHS=str(self.root.parent), MISE_AUTO_INSTALL="0", MISE_AUTO_UPDATE="0",
            GIT_CONFIG_GLOBAL=str(self.root / "gitconfig"), GIT_CONFIG_NOSYSTEM="1", GIT_ALLOW_PROTOCOL="file",
        )
        (self.root / "system").mkdir()
        (self.root / "gitconfig").write_text(
            '[user]\nname = Dots Fixture\nemail = fixture@example.invalid\n'
            f'[url "{self.root / "remote.git"}"]\ninsteadOf = https://github.com/fixture/packages.git\n'
        )

    def executable(self, name: str, content: str) -> None:
        program = self.bin / name
        program.write_text(content)
        program.chmod(0o755)

    def run_command(self, cwd: Path, *args: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
        return subprocess.run(args, cwd=cwd, env=env or self.env, stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, timeout=90, check=False)

    def succeeded(self, result: subprocess.CompletedProcess[str]) -> str:
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout

    def git(self, cwd: Path, *args: str) -> str:
        return self.succeeded(self.run_command(cwd, "git", *args)).strip()

    def on_host(self, host: str, *args: str, platform: str = "linux", **env: str) -> subprocess.CompletedProcess[str]:
        home = self.root / f"home-{host}"
        home.mkdir(exist_ok=True)
        config = self.root / host
        return self.run_command(config, *args, env={
            **self.env, "FIXTURE_OS": platform, "HOME": str(home),
            "DOTS_ROOT": str(config), "MISE_CONFIG_DIR": str(config),
            "XDG_CONFIG_HOME": str(home / ".config"), "XDG_STATE_HOME": str(home / "state"),
            "XDG_CACHE_HOME": str(home / "cache"), "XDG_DATA_HOME": str(home / "data"), **env,
        })

    def dots(self, host: str, *args: str, **env: str) -> subprocess.CompletedProcess[str]:
        return self.on_host(host, str(MANAGER / "bin/dots"), *args, **env)

    def calls(self, host: str) -> list[list[str]]:
        log = self.root / f"home-{host}/packages.log"
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []

    def installed(self, host: str) -> set[str]:
        return {item.name for item in (self.root / f"home-{host}/installed").glob("*")}

    def config(self, host: str) -> dict:
        with (self.root / host / "config.toml").open("rb") as stream:
            return tomllib.load(stream)

    def declarations(self, host: str) -> dict:
        return self.config(host).get("bootstrap", {}).get("packages", {})

    def local_tool(self) -> None:
        plugin = self.root / "plugin"
        scripts = plugin / "bin"
        scripts.mkdir(parents=True)
        (scripts / "list-all").write_text("#!/bin/sh\necho 1.0.0\n")
        (scripts / "install").write_text("""#!/bin/sh
set -eu
mkdir -p "$ASDF_INSTALL_PATH/bin"
printf '#!/bin/sh\necho fixture-tool\n' >"$ASDF_INSTALL_PATH/bin/fixture-tool"
chmod +x "$ASDF_INSTALL_PATH/bin/fixture-tool"
""")
        for script in scripts.iterdir():
            script.chmod(0o755)
        self.git(plugin, "init")
        self.git(plugin, "add", "bin")
        self.git(plugin, "commit", "-m", "Fixture tool plugin")
        for host in ("a", "b"):
            self.succeeded(self.on_host(host, "mise", "plugins", "install", "fixture-tool", plugin.as_uri()))

    def tool(self, host: str) -> Path:
        return self.root / f"home-{host}/data/mise/installs/fixture-tool/1.0.0/bin/fixture-tool"

    def test_add_sync_up_and_repeat(self) -> None:
        self.git(self.root, "init", "--bare", "--initial-branch=main", "remote.git")
        self.git(self.root, "clone", "https://github.com/fixture/packages.git", "a")
        a = self.root / "a"
        (a / "config.toml").write_text(BASE.lstrip())
        shutil.copyfile(MANAGER / "init/gitignore", a / ".gitignore")
        self.git(a, "add", "config.toml", ".gitignore")
        self.git(a, "commit", "-m", "Initial config")
        self.git(a, "push", "origin", "main")
        self.git(a, "switch", "-c", "dev")
        self.git(a, "push", "origin", "dev")
        self.git(self.root, "clone", "--branch", "dev", "https://github.com/fixture/packages.git", "b")

        self.local_tool()
        package = "apt:fixture-package"
        tool = "fixture-tool@1.0.0"
        self.assertIn(f"Recorded {package}", self.succeeded(self.dots("a", "add", "--base", tool, package)))
        self.assertEqual(self.declarations("a"), {package: "latest"})
        self.assertEqual(self.installed("a"), {package})
        self.assertTrue(self.tool("a").is_file())
        self.assertEqual(self.config("a")["tools"], {"fixture-tool": "1.0.0"})
        before = (a / "config.toml").read_bytes()
        self.succeeded(self.dots("a", "add", "--base", tool, package))
        self.assertEqual((a / "config.toml").read_bytes(), before)

        self.assertRegex(self.succeeded(self.dots("a", "sync")), r"pushed [0-9a-f]{7}")
        self.assertEqual(self.installed("b"), set())
        self.assertFalse(self.tool("b").exists())
        self.succeeded(self.dots("b", "up", "--verbose"))
        self.assertEqual(self.declarations("b"), {package: "latest"})
        self.assertEqual(self.installed("b"), {package})
        self.assertEqual(self.succeeded(self.run_command(self.root, str(self.tool("b")))), "fixture-tool\n")
        installed = self.root / "home-b/installed" / package
        first_install = installed.stat().st_mtime_ns
        first_tool_install = self.tool("b").stat().st_mtime_ns
        self.succeeded(self.dots("b", "up", "--verbose"))
        self.assertEqual(installed.stat().st_mtime_ns, first_install)
        self.assertEqual(self.tool("b").stat().st_mtime_ns, first_tool_install)
        self.assertEqual(self.git(self.root / "b", "status", "--porcelain"), "")

        failure = self.dots("a", "add", "--base", "apt:fixture-second", "apt:fixture-broken")
        self.assertNotEqual(failure.returncode, 0)
        self.assertIn("request was not recorded (noninteractive run)", failure.stderr)
        self.assertEqual(self.declarations("a"), {package: "latest", "apt:fixture-second": "latest"})
        self.assertEqual(self.installed("a"), {package, "apt:fixture-second"})

    def test_empty_host_never_calls_a_package_manager(self) -> None:
        config = self.root / "desktop"
        config.mkdir()
        (config / "config.toml").write_text(BASE.lstrip())
        self.succeeded(self.dots("desktop", "up", "--verbose", platform="macos", DOTS_SOURCE_SYNC_DONE="1"))
        self.assertEqual(self.calls("desktop"), [])

        (config / "config.toml").write_text(
            BASE.lstrip() + '\n[settings.system_packages]\nmanagers = []\n'
            + '[bootstrap.packages]\n"brew:disabled-formula" = "latest"\n'
        )
        self.succeeded(self.dots("desktop", "up", "--verbose", platform="macos", DOTS_SOURCE_SYNC_DONE="1"))
        self.assertEqual(self.calls("desktop"), [])

    def test_routine_packages_respect_exception_holds(self) -> None:
        config = self.root / "desktop"
        (config / "dots/exceptions").mkdir(parents=True)
        (config / "config.toml").write_text(
            BASE.lstrip() + '\n[bootstrap.packages]\n"apt:held" = "latest"\n"apt:regular" = "latest"\n'
        )
        (config / "dots/exceptions.tsv").write_text("desktop\towner\thold=apt:held\n")
        recipe = config / "dots/exceptions/owner"
        recipe.write_text("#!/bin/sh\nexit 0\n")
        recipe.chmod(0o755)
        self.succeeded(self.dots("desktop", "up", "--verbose", DOTS_SOURCE_SYNC_DONE="1"))
        self.assertEqual(self.installed("desktop"), {"apt:regular"})
        actions = [call for call in self.calls("desktop") if call[0] in ("apply", "upgrade")]
        self.assertEqual([call[0] for call in actions], ["apply", "upgrade"])
        self.assertTrue(all("apt:regular" in call and "apt:held" not in call for call in actions), actions)
        (config / "dots/exceptions.tsv").write_text("server\towner\thold=apt:held\n")
        self.succeeded(self.dots("desktop", "up", "--verbose", DOTS_SOURCE_SYNC_DONE="1"))
        self.assertEqual(self.installed("desktop"), {"apt:held", "apt:regular"})

    @unittest.skipUnless(sys.platform == "darwin", "Homebrew package filters apply on macOS")
    def test_cask_discovery_respects_toml_forms_profiles_and_filters(self) -> None:
        config = self.root / "desktop"
        config.mkdir()
        (config / "config.toml").write_text(
            '[bootstrap]\npackages = { "brew-cask:inline-app" = "latest", '
            '"brew-cask:auto-updated" = "latest", "brew-cask:manually-updated" = "latest", '
            '"mas:456" = "latest" }\n'
            + BASE
        )
        (config / "config.desktop.toml").write_text(
            'bootstrap.packages."brew-cask:dotted-app" = { os = "macos" }\n'
        )
        (config / "config.host-fixture.toml").write_text(
            'bootstrap.packages."brew-cask:filtered-app" = { os = "linux" }\n'
        )
        (config / "config.server.toml").write_text(
            'bootstrap.packages."brew-cask:server-app" = "latest"\n'
        )
        events = self.root / "events.jsonl"
        self.succeeded(self.on_host(
            "desktop", "bash", "-c", 'exec 3>"$1"; shift; exec "$@"', "fixture", str(events),
            str(MANAGER / "bin/dots"), "--backend", "up", "--verbose", platform="macos",
            DOTS_UP_EVENT_FD="3", DOTS_SOURCE_SYNC_DONE="1",
        ))

        self.assertEqual(self.installed("desktop"), {
            "brew-cask:inline-app", "brew-cask:dotted-app", "brew-cask:auto-updated", "brew-cask:manually-updated",
        })
        calls = self.calls("desktop")
        self.assertNotIn(["mas-probe"], calls)
        actions = [call for call in calls if call[0] in ("apply", "upgrade")]
        self.assertEqual([action[0] for action in actions], ["apply", "upgrade"], actions)
        self.assertTrue(all("brew-cask" in action for action in actions), actions)
        self.assertEqual({item for item in actions[1] if item.startswith("brew-cask:")},
                         {"brew-cask:inline-app", "brew-cask:dotted-app"})
        changes = [json.loads(line) for line in events.read_text().splitlines()]
        self.assertEqual({change["item"] for change in changes
                          if change["kind"] == "change" and change["action"] == "installed"},
                         self.installed("desktop"))

    @unittest.skipUnless(sys.platform == "darwin", "Mac App Store package stages apply on macOS")
    def test_app_store_requires_opt_in_and_authorization(self) -> None:
        config = self.root / "desktop"
        config.mkdir()
        (config / "config.toml").write_text(BASE.lstrip() + '\n[bootstrap.packages]\n"mas:123" = "latest"\n')
        self.succeeded(self.dots("desktop", "up", "--verbose", platform="macos", DOTS_SOURCE_SYNC_DONE="1"))
        self.assertEqual(self.calls("desktop"), [])
        self.assertEqual(self.installed("desktop"), set())

        deferred = self.succeeded(self.dots("desktop", "up", "--verbose", "--mas",
                                            platform="macos", DOTS_SOURCE_SYNC_DONE="1"))
        self.assertIn("authorization", deferred)
        self.assertNotIn("mas:123", self.installed("desktop"))

        log = self.root / "home-desktop/packages.log"
        log.unlink(missing_ok=True)
        self.succeeded(self.dots("desktop", "up", "--verbose", "--mas",
                                 platform="macos", DOTS_SOURCE_SYNC_DONE="1", FIXTURE_SUDO="1"))
        self.assertIn("mas:123", self.installed("desktop"))
        operations = [call[0] for call in self.calls("desktop") if "mas" in call and call[0] in ("apply", "upgrade")]
        self.assertEqual(operations, ["apply", "upgrade"])

    @unittest.skipUnless(sys.platform == "darwin", "Mac App Store package stages apply on macOS")
    def test_mas_resolves_bare_packages_and_respects_holds(self) -> None:
        config = self.root / "desktop"
        (config / "dots/exceptions").mkdir(parents=True)
        (config / "config.toml").write_text(
            BASE.lstrip() + '\n[settings.system_packages]\nprefer = ["mas"]\n'
            '[bootstrap.packages]\n"123" = "latest"\n"124" = "latest"\n'
        )
        (config / "dots/exceptions.tsv").write_text("desktop\towner\thold=mas:124\n")
        recipe = config / "dots/exceptions/owner"
        recipe.write_text("#!/bin/sh\nexit 0\n")
        recipe.chmod(0o755)
        log = self.root / "home-desktop/mas.log"
        env = {"DOTS_SOURCE_SYNC_DONE": "1", "FIXTURE_NATIVE_MAS": "1", "FIXTURE_SUDO": "1"}
        self.succeeded(self.dots("desktop", "up", "--verbose", platform="macos", **env))
        self.assertFalse(log.exists())
        self.succeeded(self.dots("desktop", "up", "--verbose", "--mas", platform="macos", **env))
        commands = log.read_text().splitlines()
        self.assertIn("install 123", commands)
        self.assertIn("upgrade 123", commands)
        self.assertTrue(all("124" not in command for command in commands), commands)

    @unittest.skipUnless(sys.platform == "darwin", "Mac App Store package stages apply on macOS")
    def test_mas_tool_formula_keeps_its_normal_brew_pass(self) -> None:
        config = self.root / "desktop"
        config.mkdir()
        (config / "config.toml").write_text(BASE.lstrip() + '\n[bootstrap.packages]\n"brew:mas" = "latest"\n')
        self.succeeded(self.dots("desktop", "up", "--verbose", platform="macos", DOTS_SOURCE_SYNC_DONE="1"))
        self.assertEqual(self.installed("desktop"), {"brew:mas"})
        self.assertFalse((self.root / "home-desktop/mas.log").exists())

    @unittest.skipUnless(sys.platform == "darwin", "Mac App Store package stages apply on macOS")
    def test_mas_defers_when_the_tool_and_brew_are_unavailable(self) -> None:
        (self.bin / "mas").unlink()
        (self.bin / "jq").symlink_to(shutil.which("jq"))
        config = self.root / "desktop"
        config.mkdir()
        for managers, mise_platform in (('["mas"]', "macos-arm64"), ('["mas", "brew"]', "macos-x64")):
            with self.subTest(managers=managers, mise_platform=mise_platform):
                (config / "config.toml").write_text(
                    BASE.lstrip() + f'\n[settings.system_packages]\nmanagers = {managers}\n'
                    '[bootstrap.packages]\n"mas:123" = "latest"\n'
                )
                output = self.succeeded(self.dots(
                    "desktop", "up", "--verbose", "--mas",
                    platform="macos", DOTS_SOURCE_SYNC_DONE="1", FIXTURE_SUDO="1",
                    FIXTURE_ARCH="arm64", FIXTURE_MISE_PLATFORM=mise_platform, PATH=f"{self.bin}:/usr/bin:/bin",
                ))
                self.assertIn("needs an installed mas tool", output)
                self.assertEqual(self.calls("desktop"), [])

    @unittest.skipUnless(sys.platform == "darwin", "Mac App Store package stages apply on macOS")
    def test_mas_can_use_the_brew_tool_outside_path(self) -> None:
        (self.bin / "jq").symlink_to(shutil.which("jq"))
        prefix = self.root / "mas-opt"
        (prefix / "bin").mkdir(parents=True)
        (self.bin / "mas").rename(prefix / "bin/mas")
        (self.bin / "mas").symlink_to(self.env["FIXTURE_MISE"])
        config = self.root / "desktop"
        config.mkdir()
        (config / "config.toml").write_text(BASE.lstrip() + '\n[bootstrap.packages]\n"mas:123" = "latest"\n')
        env = {"DOTS_SOURCE_SYNC_DONE": "1", "FIXTURE_SUDO": "1", "FIXTURE_MISE_PLATFORM": "macos-arm64",
               "PATH": f"{self.bin}:/usr/bin:/bin", "FIXTURE_NATIVE_MAS": "1"}
        output = self.succeeded(self.dots("desktop", "up", "--verbose", "--mas",
                                          platform="macos", **env))
        self.assertIn("needs an installed mas tool", output)
        log = self.root / "home-desktop/mas.log"
        self.assertFalse(log.exists())
        (self.root / "home-desktop/installed/brew:mas").unlink()
        self.succeeded(self.dots("desktop", "up", "--verbose", "--mas", platform="macos",
                                 FIXTURE_BREW_MAS_PREFIX=str(prefix), **env))
        self.assertIn("install 123", log.read_text().splitlines())
        log.unlink()
        self.succeeded(self.dots("desktop", "up", "--verbose", "--mas", platform="macos",
                                 FIXTURE_BREW_MAS_PREFIX=str(prefix), **env))
        self.assertIn("upgrade 123", log.read_text().splitlines())
    @unittest.skipUnless(sys.platform == "darwin", "Mac App Store package queries apply on macOS")
    def test_status_and_plan_do_not_probe_the_app_store(self) -> None:
        config = self.root / "desktop"
        config.mkdir()
        (config / "config.toml").write_text(BASE.lstrip() + '\n[bootstrap.packages]\n"mas:123" = "latest"\n')
        self.git(config, "init", "--initial-branch=dev")
        self.git(config, "add", "config.toml")
        self.git(config, "commit", "-m", "Fixture settings")
        self.git(config, "remote", "add", "origin", "https://github.com/fixture/packages.git")

        commands = {
            "plain status": (str(MANAGER / "bin/dots"), "status"),
            "structured status": (sys.executable, str(MANAGER / "lib/dots-command-backend.py"),
                                  str(self.bin / "mise"), str(config), "status"),
            "plan": (str(MANAGER / "bin/dots"), "plan"),
        }
        for name, command in commands.items():
            with self.subTest(command=name):
                result = self.on_host("desktop", *command, platform="macos", DOTS_UP_STREAM_NONCE="fixture-status")
                log = self.root / "home-desktop/mas.log"
                self.assertFalse(log.exists(), log.read_text() if log.exists() else "")
                output = self.succeeded(result)
                if name == "structured status":
                    self.assertIn('"id":"setup","label":"Declared setup","status":"ok"', output)
