"""Exercise native completion through interactive shells with a mock dots."""

import fcntl
import json
import os
import pty
import select
import shlex
import shutil
import subprocess
import sys
import termios
import tempfile
import time
import unittest
from pathlib import Path


COMPLETIONS = Path(__file__).resolve().parents[1] / "completions"
SHELL_FILES = {"bash": "dots.bash", "fish": "dots.fish", "zsh": "_dots"}

MOCK = r"""
import json
import os
import sys
from pathlib import Path

args = sys.argv[1:]
if args[:1] == ["completions"]:
    sys.stdout.write(Path(os.environ["DOTS_ADAPTER"]).read_text())
    raise SystemExit

if args[:2] == ["internal", "shell-complete"]:
    for key in ("MISE_SAFE", "MISE_NO_ENV", "MISE_NO_HOOKS", "MISE_OFFLINE"):
        assert os.environ.get(key) == "1", key
    for key in ("MISE_AUTO_INSTALL", "MISE_EXEC_AUTO_INSTALL", "MISE_AUTO_UPDATE"):
        assert os.environ.get(key) == "0", key

with open(os.environ["DOTS_CALLS"], "a") as log:
    print(json.dumps(args), file=log)

if args[:2] != ["internal", "shell-complete"]:
    raise SystemExit

words = args[2:]
if len(words) > 2:
    raise SystemExit
if words[0] == "file":
    print(":files")
elif words[0] == "dir":
    print(":dirs")
else:
    for value in ["add", "brew-cask:", "brew-cask:codexbar", "two words"]:
        if value.startswith(words[-1]):
            # Offer backend names before the colon, and packages after it.
            if value == "brew-cask:" and words[-1].endswith(":"):
                continue
            if value == "brew-cask:codexbar" and ":" not in words[-1]:
                continue
            print(value + "\tfixture description")
"""


class ShellCompletions(unittest.TestCase):
    def run_completion(self, shell, keys, *, autoload=False):
        binary = shutil.which(shell)
        if binary is None:
            self.skipTest(f"{shell} is unavailable")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            dots = root / "dots"
            dots.write_text(f"#!{sys.executable}\n" + MOCK)
            dots.chmod(0o755)
            (root / "env=prod").mkdir()
            (root / "env=prod/config.toml").touch()
            (root / "config\tlocal.toml").touch()
            (root / "path file").touch()
            (root / "path folder").mkdir()
            log = root / "calls"
            env = {
                "PATH": f"{root}{os.pathsep}{os.defpath}",
                "HOME": str(root),
                "ZDOTDIR": str(root),
                "TERM": "dumb",
                "LC_ALL": "C",
                "DOTS_ADAPTER": str(COMPLETIONS / SHELL_FILES[shell]),
                "DOTS_CALLS": str(log),
            }
            if shell == "bash":
                flags = ["--noprofile", "--norc", "-i"]
                setup = 'eval "$(dots completions bash)"'
            elif shell == "fish":
                flags = ["--no-config", "-i"]
                setup = "dots completions fish | source"
            elif autoload:
                flags = ["-f", "-i"]
                setup = (
                    f"fpath=({shlex.quote(str(COMPLETIONS))} $fpath); "
                    "autoload -Uz compinit; compinit -D -i"
                )
            else:
                flags = ["-f", "-i"]
                setup = (
                    "source <(dots completions zsh) && "
                    "autoload -Uz compinit && compinit -D -i && "
                    "source <(dots completions zsh)"
                )

            master, slave = pty.openpty()
            def attach_terminal():
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)

            process = subprocess.Popen(
                [binary, *flags], stdin=slave, stdout=slave, stderr=slave,
                cwd=root, env=env, preexec_fn=attach_terminal,
            )
            os.close(slave)
            output = bytearray()

            def wait_for(marker):
                deadline = time.monotonic() + 5
                while marker not in output:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or not select.select([master], [], [], remaining)[0]:
                        self.fail(f"{shell} did not reach {marker!r}: {output!r}")
                    try:
                        output.extend(os.read(master, 65536))
                    except OSError as exc:
                        self.fail(f"{shell} exited early: {exc}: {output!r}")

            try:
                # Split the marker so the terminal echo cannot satisfy the wait.
                os.write(master, (setup + " && printf '%s%s\\n' READY _DOTS\n").encode())
                wait_for(b"READY_DOTS")
                os.write(master, keys + b"\n")
                os.write(master, b"printf '%s%s\\n' DONE _DOTS\n")
                wait_for(b"DONE_DOTS")
            finally:
                process.kill()
                os.close(master)
                process.wait(timeout=5)
            self.assertTrue(log.exists(), bytes(output))
            return [json.loads(line) for line in log.read_text().splitlines()]

    def assert_completion(self, shell, keys, words, executed, **kwargs):
        calls = self.run_completion(shell, keys, **kwargs)
        completion = ["internal", "shell-complete", *words]
        self.assertIn(completion, calls)
        self.assertEqual(calls[-1], executed, calls)

    def test_empty_current_and_quoted_arguments(self):
        for shell in SHELL_FILES:
            with self.subTest(shell=shell):
                self.assert_completion(
                    shell, b'dots "a b" "" brew-cask:codexbar \t',
                    ["a b", "", "brew-cask:codexbar", ""],
                    ["a b", "", "brew-cask:codexbar"],
                )

    def test_backend_prefix_and_colon_word_break(self):
        for shell in SHELL_FILES:
            with self.subTest(shell=shell):
                calls = self.run_completion(shell, b"dots add brew-c\tcod\t")
                self.assertIn(["internal", "shell-complete", "add", "brew-c"], calls)
                self.assertIn(["internal", "shell-complete", "add", "brew-cask:cod"], calls)
                self.assertEqual(calls[-1], ["add", "brew-cask:codexbar"])

    def test_space_and_cursor_in_existing_command(self):
        for shell in SHELL_FILES:
            with self.subTest(shell=shell):
                self.assert_completion(
                    shell, b"dots add two later" + b"\x02" * 6 + b"\t",
                    ["add", "two"], ["add", "two words", "later"],
                )

    def test_native_file_and_directory_completion(self):
        for shell in SHELL_FILES:
            with self.subTest(shell=shell):
                self.assert_completion(
                    shell, b"dots file path\\ fi\t",
                    ["file", "path fi"], ["file", "path file"],
                )
                self.assert_completion(
                    shell, b"dots dir pa\tchild",
                    ["dir", "pa"], ["dir", "path folder/child"],
                )

    def test_inline_path_containing_equals(self):
        for shell in SHELL_FILES:
            with self.subTest(shell=shell):
                self.assert_completion(shell, b"dots file --path=env=prod/con\t",
                                       ["file", "--path=env=prod/con"],
                                       ["file", "--path=env=prod/config.toml"])

    def test_bash_preserves_tab_in_filename(self):
        self.assert_completion("bash", b"dots file config\t", ["file", "config"], ["file", "config\tlocal.toml"])

    def test_bash_prior_home_path(self):
        for source, expand in [(b"~/chosen", True), (b"'~/chosen'", False), (b"\\~/chosen", False)]:
            with self.subTest(source=source):
                calls = self.run_completion("bash", b"dots remove --path " + source + b" rip\t")
                self.assertEqual(calls[0][4], calls[-1][2])
                self.assertEqual(os.path.isabs(calls[0][4]), expand)

    def test_zsh_autoload(self):
        self.assert_completion("zsh", b"dots a\t", ["a"], ["add"], autoload=True)


if __name__ == "__main__":
    unittest.main()
