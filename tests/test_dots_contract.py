"""The configuration contract: hooks, commands, installer exceptions and settings.

Every test runs a copy of the manager against a fixture configuration with a
fake mise, a temporary HOME and no network. Nothing touches the real machine.
"""

from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import textwrap
import tomllib
import unittest
from pathlib import Path

from lib.prereq import mise_binary, requires

MANAGER = Path(__file__).resolve().parents[1]

# A fake mise: logs every call, answers [vars] from FAKE_VAR_<name> or else
# from the string in the --file it was given, runs `exec -- COMMAND` directly,
# simulates source sync with FAKE_SYNC, and succeeds for every other
# subcommand without doing anything. For settings history it answers
# `dotfiles paths --json` with FAKE_PATHS (default no entries), a dotfiles.*
# key from the TOML in its --file, the effective history status with
# FAKE_HISTORY_STATUS and `settings get` with FAKE_SETTING.
FAKE_MISE = r"""#!/usr/bin/env bash
printf '%s\n' "$*" >> "$FAKE_MISE_LOG"
if [[ "${1:-} ${2:-}" == '--quiet --no-config' && -n "${FIXTURE_TOML_MISE:-}" ]]; then
    exec "$FIXTURE_TOML_MISE" "$@"
fi
while [[ "${1:-}" == -C || "${1:-}" == -E ]]; do shift 2; done
case "${1:-}" in
    config)
        if [[ "${2:-}" == ls && -n "${FAKE_CONFIG_FAILURE:-}" ]]; then
            printf '%s\n' "$FAKE_CONFIG_FAILURE" >&2
            exit 1
        fi
        if [[ "${2:-}" == get ]]; then
            key="${5:-}"
            case "$key" in
                vars.*)
                    variable="FAKE_VAR_${key#vars.}"
                    if [[ -n "${!variable+set}" ]]; then printf '%s\n' "${!variable}"; exit 0; fi
                    [[ "${3:-}" == --file && -f "${4:-}" ]] || exit 1
                    exec python3 -c '
import sys, tomllib
value = tomllib.load(open(sys.argv[1], "rb")).get("vars", {}).get(sys.argv[2])
sys.exit(1) if not isinstance(value, str) else print(value)
' "$4" "${key#vars.}" ;;
                dotfiles.*)
                    [[ "${3:-}" == --file && -f "${4:-}" ]] || exit 1
                    exec python3 -c '
import sys, tomllib
value = tomllib.load(open(sys.argv[1], "rb"))
path = tomllib.loads(sys.argv[2] + " = 0")  # the dotted key, parsed as TOML
while isinstance(path, dict):
    (part, path), = path.items()
    if not isinstance(value, dict) or part not in value: sys.exit(1)
    value = value[part]
print(str(value).lower() if isinstance(value, bool) else "table" if isinstance(value, dict) else value)
' "$4" "$key" ;;
                *) exit 1 ;;
            esac
        fi
        exit 0 ;;
    exec)
        shift
        [[ "${1:-}" == -- ]] && shift
        for ((i = 1; i <= $#; i++)); do
            if [[ "${!i}" == */source-repo.mjs && -z "${FIXTURE_REAL_SYNC:-}" ]]; then
                # Keep any wrapper before node (env ...), so FAKE_SYNC sees sync's environment.
                [[ -z "${FAKE_SYNC:-}" ]] || exec "${@:1:i-2}" bash -c "$FAKE_SYNC"
                exit 0
            fi
        done
        exec "$@" ;;
    bootstrap)
        none='{"entries": []}'
        empty='{}'
        case "$*" in
            'bootstrap packages status --json') printf '%s\n' "$empty" ;;
            'bootstrap dotfiles paths --json') printf '%s\n' "${FAKE_PATHS-$none}" ;;
            'bootstrap dotfiles status --json') printf '%s\n' "${FAKE_HISTORY_STATUS-$empty}" ;;
        esac
        ;;
    ls) printf '{}\n' ;;
    settings) [[ "${2:-}" == get && -n "${FAKE_SETTING:-}" ]] || exit 1; printf '%s\n' "$FAKE_SETTING" ;;
    which) exit 1 ;;
    *) exit 0 ;;
esac
"""


def history_paths(home: Path, *paths: str, variant: str = "host-fx", encrypt: bool = True,
                  absolute: bool = False) -> str:
    """Declare each path in HOME's config.host-fx.toml (by its absolute target with
    ABSOLUTE) and return what `mise dotfiles paths --json` reports for them."""
    scope = f', variants = [{{ profile = "{variant}" }}]' if variant else ""
    keys = [str(home / path[2:]) if absolute else path for path in paths]
    write(home / ".config/mise/config.host-fx.toml", "[dotfiles]\n" + "".join(
        f'{json.dumps(key)} = {{ mode = "track", encrypt = {str(encrypt).lower()}{scope} }}\n' for key in keys))
    entries = [{"path": path, "mode": "track", "declared_in": "~/.config/mise/config.host-fx.toml",
                **({"variant": variant} if variant else {})} for path in paths]
    return json.dumps({"entries": entries})


def write(path: Path, text: str, mode: int = 0o644) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(text).lstrip())
    path.chmod(mode)
    return path


def remove_git_marker(manager: Path) -> None:
    """Drop the copied .git: a directory in a clone, a file in a linked worktree."""
    marker = manager / ".git"
    if marker.is_dir() and not marker.is_symlink():
        shutil.rmtree(marker)
    elif marker.exists() or marker.is_symlink():
        marker.unlink()


class Fixture:
    """A manager copy, a configuration, a fake mise and a private HOME."""

    def __init__(self, base: Path, *, profiles: str | None = "laptop server") -> None:
        self.base = base
        self.manager = base / "manager"
        shutil.copytree(MANAGER, self.manager, ignore=shutil.ignore_patterns("dots-up", "tests", "__pycache__"))
        self.config = base / "config"
        self.config.mkdir()
        (self.config / "config.toml").write_text("# minimal configuration\n")
        self.home = base / "home"
        self.home.mkdir()
        self.log = base / "mise.log"
        self.log.touch()
        self.mise = write(base / "bin/mise", FAKE_MISE, 0o755)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("DOTS_", "MISE_", "SETUP_", "XDG_"))}
        self.env.update(HOME=str(self.home), DOTS_ROOT=str(self.config), DOTS_HOME=str(self.manager),
                        SETUP_MISE_BIN=str(self.mise), DOTS_MISE_BIN=str(self.mise), FAKE_MISE_LOG=str(self.log),
                        DOTS_UP_BIN="/nonexistent/dots-up", DOTS_SKILL_LINKS="0")
        if profiles is not None:
            self.env["FAKE_VAR_dots_profiles"] = profiles

    def hook(self, kind: str, name: str, body: str, mode: int = 0o755) -> Path:
        return write(self.config / "dots/hooks" / kind / name, "#!/usr/bin/env bash\n" + textwrap.dedent(body), mode)

    def update(self, *args: str, **env: str) -> subprocess.CompletedProcess[str]:
        environment = {**self.env, "SETUP_PROFILE": "laptop", "DOTS_SOURCE_SYNC_DONE": "1", **env}
        return subprocess.run(["bash", str(self.manager / "tasks/update"), *args], cwd=self.config,
                              env=environment, capture_output=True, text=True, timeout=120, check=False)

    def dots(self, *args: str, cwd: Path | None = None, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run([str(self.manager / "bin/dots"), *args], cwd=cwd or self.config,
                              env={**self.env, **env}, capture_output=True, text=True, timeout=60, check=False)

    def bash(self, script: str, **env: str) -> subprocess.CompletedProcess[str]:
        prelude = f'source "{self.manager}/lib/setup-common.sh"\n'
        return subprocess.run(["bash", "-c", prelude + script], cwd=self.config,
                              env={**self.env, "SETUP_PROFILE": "laptop", **env},
                              capture_output=True, text=True, timeout=60, check=False)


class ContractTestCase(unittest.TestCase):
    profiles: str | None = "laptop server"

    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="dots-contract-")
        self.addCleanup(temporary.cleanup)
        self.f = Fixture(Path(temporary.name).resolve(), profiles=self.profiles)


@requires("jq")
class UpdateHookTests(ContractTestCase):
    def test_invalid_update_config_emits_one_failed_stage_with_diagnostic(self) -> None:
        result = self.f.update(DOTS_UP_STREAM_NONCE="fixture", FAKE_CONFIG_FAILURE="mise: invalid fixture setting")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("mise: invalid fixture setting", result.stderr)
        events = [json.loads(frame.split(":", 2)[2]) for frame in result.stdout.split("\x1f")
                  if frame.startswith("\x1edots-up/1:fixture:")]
        self.assertEqual([event["kind"] for event in events], ["run_start", "stage_start", "stage_end"])
        self.assertEqual(events[1]["id"], events[2]["id"])
        self.assertEqual(events[2]["status"], "failed")
        self.assertIn("rerun dots up", events[2]["action"])

    def test_update_finishes_on_its_manager_and_reports_a_new_one(self) -> None:
        # A packaged manager (no .git) that a stage upgraded: the newer one is
        # first on PATH, the run keeps going on the old one, and Homebrew may
        # not delete the old keg meanwhile.
        remove_git_marker(self.f.manager)
        (self.f.manager / "VERSION").write_text("0.3.0\n")
        newer = self.f.base / "installs/dots/0.3.1"
        shutil.copytree(self.f.manager, newer)
        (newer / "VERSION").write_text("0.3.1\n")
        out = self.f.base / "cleanup"
        self.f.hook("update.d", "10-env", f'printf "%s %s" "$HOMEBREW_NO_INSTALL_CLEANUP" "$DOTS_HOME" > {out}\n')
        result = self.f.update(PATH=f"{newer}/bin:{self.f.env['PATH']}")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(out.read_text(), f"1 {self.f.manager}")
        self.assertIn("== dots manager ==\ndots 0.3.0 -> 0.3.1, used from the next run.\nResult: completed", result.stdout)

    def test_update_skips_self_update_for_a_package_managed_mise(self) -> None:
        # Homebrew's mise ships lib/.disable-self-update and refuses self-update.
        package = self.f.base / "Cellar/mise/2026.11.7"
        head, tail = FAKE_MISE.rsplit("    *) exit 0 ;;", 1)
        refusing = head + "    self-update) echo 'mise: self-update is disabled' >&2; exit 1 ;;\n    *) exit 0 ;;" + tail
        managed = write(package / "bin/mise", refusing, 0o755)
        write(package / "lib/.disable-self-update", "")
        result = self.f.update(SETUP_MISE_BIN=str(managed), DOTS_MISE_BIN=str(managed))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Skipped: standalone mise is updated by the package manager that installed it", result.stdout)
        self.assertNotIn("self-update", self.f.log.read_text())

    def test_update_leaves_a_checkout_manager_alone(self) -> None:
        remove_git_marker(self.f.manager)
        (self.f.manager / ".git").mkdir()
        result = self.f.update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertRegex(result.stdout, r"== dots manager ==\ndots \S+ runs from a checkout; update it with git\.\n")

    def test_hooks_run_in_name_order_with_contract_environment_and_statuses(self) -> None:
        out = self.f.base / "out"
        out.mkdir()
        # Created out of order; dispatch follows the names.
        self.f.hook("update.d", "50-after-failure", f"touch {out}/after\n")
        self.f.hook("update.d", "40-broken", "echo broken-output; exit 7\n")
        self.f.hook("update.d", "30-skipped.sh", "echo 'Skipped: not here'; exit 4\n")
        self.f.hook("update.d", "20-Deferred_step", "exit 3\n")
        self.f.hook("update.d", "10-first", f"""
            printf '%s\\n' "$DOTS_ROOT" "$DOTS_HOME" "$DOTS_PROFILE" "$DOTS_TOOLS_OK" "$DOTS_TOOLS_UPGRADED" "$PWD" > {out}/env
        """)
        self.f.hook("update.d", "60-not-executable", f"touch {out}/not-executable\n", mode=0o644)
        self.f.hook("update.d", "70 odd name", f"touch {out}/odd\n")
        self.f.hook("update.d", "45-laptop-only@laptop", "true\n")
        self.f.hook("update.d", "46-server-only@server", f"touch {out}/server\n")
        (self.f.config / "dots/hooks/update.d/80-directory").mkdir()

        result = self.f.update()

        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        labels = [line[3:-3] for line in result.stdout.splitlines() if line.startswith("== ")]
        hooks = labels[labels.index("first"):labels.index("standalone mise")]
        self.assertEqual(hooks, ["first", "Deferred step", "skipped", "broken", "laptop only", "after failure"])
        self.assertIn("== Deferred step ==\nResult: deferred; needs attention", result.stdout)
        self.assertIn("== skipped ==\nSkipped: not here\nResult: skipped", result.stdout)
        self.assertIn("== broken ==\nbroken-output\nResult: failed (exit 7)", result.stdout)
        self.assertIn("Update report: failures: broken; ", result.stdout)
        self.assertTrue((out / "after").exists(), "a later hook did not run after a failure")
        self.assertFalse((out / "not-executable").exists())
        self.assertFalse((out / "odd").exists())
        self.assertFalse((out / "server").exists())
        self.assertEqual((out / "env").read_text().splitlines(),
                         [str(self.f.config), str(self.f.manager), "laptop", "1", "1", str(self.f.config)])
        # Hooks run after installer exceptions and before standalone mise.
        self.assertLess(result.stdout.index("== declared mise tools =="), result.stdout.index("== first =="))

    def test_hook_changes_reach_the_stage_report(self) -> None:
        import json
        write(self.f.config / "dots/exceptions/recipe", "#!/usr/bin/env bash\ntrue\n", 0o755)
        write(self.f.config / "dots/exceptions.tsv", "laptop\trecipe\n")
        self.f.hook("update.d", "10-Reports-changes", r"""
            printf 'plugin-a\tabc123\tdef456\n' >> "$DOTS_CHANGES"
            printf 'no tabs here\n\none\ttwo\n\tempty\titem\na\tb\tc\td\n' >> "$DOTS_CHANGES"
            printf 'plugin-b\tnot installed\t\n' >> "$DOTS_CHANGES"
        """)
        self.f.hook("update.d", "20-Silent", '[[ -f "$DOTS_CHANGES" ]] && printf "%s" "$DOTS_CHANGES" > ' + f'{self.f.base}/path\n')
        events = self.f.base / "events"
        # The dashboard passes events on descriptor 3; recipes and hooks must not disturb it.
        result = subprocess.run(["bash", "-c", 'exec 3>>"$0"; exec bash "$1"', str(events), str(self.f.manager / "tasks/update")],
                                cwd=self.f.config, env={**self.f.env, "SETUP_PROFILE": "laptop", "DOTS_SOURCE_SYNC_DONE": "1",
                                                        "DOTS_UP_EVENT_FD": "3"},
                                capture_output=True, text=True, timeout=120, check=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        parsed = [json.loads(line) for line in events.read_text().splitlines()]
        changes = [(e["stage"], e["item"], e["before"], e["after"]) for e in parsed if e["kind"] == "change"]
        self.assertEqual(changes, [("Reports-changes", "plugin-a", "abc123", "def456"),
                                   ("Reports-changes", "plugin-b", "not installed", "")])
        self.assertIn("Note: ignored 4 malformed DOTS_CHANGES line(s)", result.stdout)
        notices = [e for e in parsed if e["kind"] == "notice" and e.get("stage") == "Reports-changes"]
        self.assertEqual(len(notices), 1)
        self.assertIn({"kind": "stage_end", "id": "installer-exception--recipe", "status": "ok"},
                      [{k: e.get(k) for k in ("kind", "id", "status")} for e in parsed])
        # Every hook gets its own file, removed after the stage.
        self.assertFalse(Path((self.f.base / "path").read_text()).exists())

    def test_unknown_profile_suffix_runs_no_hooks_and_fails(self) -> None:
        marker = self.f.base / "ran"
        self.f.hook("update.d", "10-fine", f"touch {marker}\n")
        self.f.hook("update.d", "20-typo@labtop", "true\n")
        result = self.f.update()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('names unknown profile "labtop"', result.stderr)
        self.assertIn("Update report: failures: update hooks; ", result.stdout)
        self.assertFalse(marker.exists())

    def test_deferred_hook_is_reported_without_failing_the_run(self) -> None:
        self.f.hook("update.d", "10-Later", "exit 3\n")
        result = self.f.update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Update report: completed; needs attention: Later; ", result.stdout)

    def test_failed_tools_install_is_visible_to_hooks(self) -> None:
        out = self.f.base / "tools"
        self.f.hook("update.d", "10-tools", f'printf "%s %s" "$DOTS_TOOLS_OK" "$DOTS_TOOLS_UPGRADED" > {out}\n')
        failing = write(self.f.base / "bin/failing-mise", FAKE_MISE.replace(
            "    *) exit 0 ;;\nesac", "    install) exit 9 ;;\n    *) exit 0 ;;\nesac"), 0o755)
        result = self.f.update(SETUP_MISE_BIN=str(failing))
        self.assertIn("== newly declared mise tools ==\nResult: failed (exit 9)", result.stdout)
        self.assertNotIn("== declared mise tools ==", result.stdout)
        self.assertEqual(out.read_text(), "0 0")

    def test_install_only_exceptions_run_with_the_flag(self) -> None:
        log = self.f.base / "recipes"
        for name in ("plain-app", "self-updater", "everywhere"):
            write(self.f.config / "dots/exceptions" / name, f'#!/usr/bin/env bash\necho "{name} $*" >> {log}\n', 0o755)
        write(self.f.config / "dots/exceptions.tsv",
              "# profile\tname\tflags\nlaptop\tplain-app\nlaptop\tself-updater\tinstall-only,hold=brew:thing\n"
              "server\tnot-here\nbase\teverywhere\n")
        result = self.f.update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(log.read_text().splitlines(), ["plain-app ", "self-updater --install-only", "everywhere "])
        self.assertIn("== installer exception: self-updater ==", result.stdout)

    def test_recipes_get_the_callers_stdin_not_the_manifest(self) -> None:
        log = self.f.base / "stdin"
        for name in ("reader", "after-reader"):
            write(self.f.config / "dots/exceptions" / name,
                  f'#!/usr/bin/env bash\nIFS= read -r line || line="<eof>"\necho "{name}:$line" >> {log}\n', 0o755)
        write(self.f.config / "dots/exceptions.tsv", "laptop\treader\nlaptop\tafter-reader\n")
        result = subprocess.run(["bash", str(self.f.manager / "tasks/update")], cwd=self.f.config,
                                env={**self.f.env, "SETUP_PROFILE": "laptop", "DOTS_SOURCE_SYNC_DONE": "1"},
                                input="from-terminal\nsecond-line\n", capture_output=True, text=True, timeout=120, check=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # Each recipe reads the caller's input; neither consumes a manifest row.
        self.assertEqual(log.read_text().splitlines(), ["reader:from-terminal", "after-reader:second-line"])

    def test_source_sync_that_changes_the_manager_reexecs_once_into_the_new_code(self) -> None:
        update = self.f.manager / "tasks/update"
        changed = self.f.base / "update.new"
        changed.write_text(update.read_text().replace("no updater failures.", "no updater failures (new manager)."))
        changed.chmod(0o755)
        sync = f'cp "{changed}" "{update}.tmp" && mv "{update}.tmp" "{update}"'
        result = self.f.update(DOTS_SOURCE_SYNC_DONE="0", FAKE_SYNC=sync)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Update report: completed; no updater failures (new manager).", result.stdout)
        calls = self.f.log.read_text().splitlines()
        self.assertEqual(sum("source-repo.mjs sync" in call for call in calls), 1, calls)
        reexecs = [call for call in calls if call.endswith(f"exec -- bash {self.f.manager}/tasks/update")]
        self.assertEqual(len(reexecs), 1, calls)

    def test_source_sync_keeps_the_callers_auto_install_for_the_hooks_it_applies(self) -> None:
        # The apply's hooks may `mise x` a release the incoming configuration
        # newly pins; setup_mise's MISE_AUTO_INSTALL=0 must not reach them.
        sync = "echo \"dots sync: no local changes · auto-install=${MISE_AUTO_INSTALL-unset}\""
        for caller, seen in ((None, "unset"), ("1", "1"), ("0", "0")):
            with self.subTest(caller=caller):
                result = self.f.update(DOTS_SOURCE_SYNC_DONE="0", FAKE_SYNC=sync,
                                       **({"MISE_AUTO_INSTALL": caller} if caller else {}))
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(f"auto-install={seen}\n", result.stdout)

    def test_sync_issues_and_crashes_allow_maintenance_to_continue(self) -> None:
        for code in (3, 127):
            with self.subTest(code=code):
                result = self.f.update(DOTS_SOURCE_SYNC_DONE="0", FAKE_SYNC=f"echo 'Origin unreachable'; exit {code}")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(result.stdout.count("Origin unreachable"), 1)
                self.assertIn("Update report: completed; needs attention: source sync;", result.stdout)

    @requires("node")
    def test_update_carries_typed_sync_issue_and_continues(self) -> None:
        events = self.f.base / "events"
        result = subprocess.run(["bash", "-c", 'exec 3>>"$0"; exec bash "$1"', str(events), str(self.f.manager / "tasks/update")],
                                cwd=self.f.config, env={**self.f.env, "SETUP_PROFILE": "laptop", "DOTS_SOURCE_SYNC_DONE": "0",
                                                        "DOTS_UP_EVENT_FD": "3", "FIXTURE_REAL_SYNC": "1"},
                                capture_output=True, text=True, timeout=120, check=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        parsed = [json.loads(line) for line in events.read_text().splitlines()]
        issues = [e for e in parsed if e.get("status") == "attention" and e.get("id") == "source-sync"]
        self.assertEqual(len(issues), 1)
        self.assertIn("not a Git repository", issues[0]["summary"])
        self.assertIn("dots sync", issues[0]["action"])
        self.assertIn("Update report: completed; needs attention: source sync;", result.stdout)

    def test_failed_and_deferred_stages_carry_their_output_in_plain_mode(self) -> None:
        self.f.hook("update.d", "10-Waits", "echo 'needs a login: run gh auth login'\nexit 3\n")
        self.f.hook("update.d", "20-Breaks", "echo 'step one'\necho 'boom: disk full' >&2\nexit 7\n")
        result = self.f.update("--plain")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        output = result.stdout + result.stderr
        self.assertRegex(output, r"Waits · deferred; needs attention\nRecent output:\nneeds a login: run gh auth login\nFull log: \S+/stages/Waits\.log")
        self.assertRegex(output, r"Recent output:\nstep one\nboom: disk full\nFull log: \S+/stages/Breaks\.log")
        self.assertRegex(result.stdout, r"Logs of the steps that need attention:\n  Waits: \S+Waits\.log\n  Breaks: \S+Breaks\.log")

class CommandBackendTests(ContractTestCase):
    @requires("node")
    def test_standalone_sync_attaches_its_own_stage_and_preserves_public_exit(self) -> None:
        events = self.f.base / "events"
        report_dir = self.f.base / "run"
        report_dir.mkdir()
        result = subprocess.run(["bash", "-c", 'exec 3>"$0"; exec "$@"', str(events), sys.executable,
                                 str(self.f.manager / "lib/dots-command-backend.py"), str(self.f.mise), str(self.f.config), "sync"],
                                cwd=self.f.config, env={**self.f.env, "DOTS_UP_EVENT_FD": "3", "FIXTURE_REAL_SYNC": "1",
                                                       "DOTS_UP_RUN_DIR": str(report_dir)},
                                capture_output=True, text=True, timeout=60, check=False)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("not a Git repository", result.stdout)
        preview = self.f.dots("internal", "sync-present", str(report_dir / "sync.json"))
        self.assertEqual(preview.returncode, 0, preview.stderr)
        self.assertEqual(preview.stdout.count("Could not save this sync for replay."), 1)
        parsed = [json.loads(line) for line in events.read_text().splitlines()]
        self.assertEqual([(e["id"], e["status"]) for e in parsed if e["kind"] == "stage_end"], [("sync", "attention")])
        self.assertEqual([(e["status"], e["exit_code"]) for e in parsed if e["kind"] == "run_end"], [("attention", 0)])

    def test_up_last_finds_the_latest_finished_bash_log(self) -> None:
        state = self.f.home / ".local/state/dots/up"
        for run in ("20261201", "20261202", "20261203"):
            write(state / run / ".fallback", "")
        write(state / "20261203/.active", "")
        replay = self.f.dots("up", "--last")
        self.assertEqual(replay.returncode, 0, replay.stderr)
        self.assertIn(str(state / "20261202/stages"), replay.stdout)

    @requires("git", "node", "gitleaks")
    def test_public_sync_records_changed_files_and_replays_without_syncing(self) -> None:
        self.f.env = {key: value for key, value in self.f.env.items() if not key.startswith("GIT_")}
        self.f.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_ALLOW_PROTOCOL="file",
                          FIXTURE_REAL_SYNC="1", FAKE_HISTORY_STATUS='{"files": [], "edits": []}')
        def git(*args):
            return subprocess.check_output(["git", *args], cwd=self.f.config, env=self.f.env, text=True).strip()
        git("init", "-q", "-b", "main")
        git("config", "user.name", "Fixture")
        git("config", "user.email", "fixture@example.invalid")
        write(self.f.config / ".gitignore", "/miserc.toml\n/config.local.toml\n/config.host-*.toml\n/mise.lock\n/.setup-state/\n/locks/\n")
        write(self.f.config / "config.toml", '[vars]\ndots_source_repo = "fixture/config"\n')
        git("add", ".")
        git("commit", "-qm", "base")
        if subprocess.run(["git", "merge-tree", "--write-tree", "-X", "theirs", "HEAD", "HEAD"],
                          cwd=self.f.config, env=self.f.env, capture_output=True).returncode:
            self.skipTest("sync requires Git with merge-tree -X")
        origin = str(self.f.base / "origin.git")
        git("clone", "-q", "--bare", ".", origin)
        git("remote", "add", "origin", "https://github.com/fixture/config")
        git("config", "url." + origin + ".insteadOf", "https://github.com/fixture/config")
        git("switch", "-qc", "dev")
        git("push", "-q", "origin", "dev")
        write(self.f.config / "editor.ini", "tab_size = 4\n")
        run = self.f.dots("sync")
        self.assertEqual(run.returncode, 0, run.stdout + run.stderr)
        self.assertIn("Shared on dev", run.stdout)
        self.assertIn("editor.ini", run.stdout)
        self.assertNotIn("tab_size", run.stdout)
        head = git("rev-parse", "HEAD")
        write(self.f.config / "editor.ini", "tab_size = 8\n")
        replay = self.f.dots("sync", "--last")
        self.assertEqual(replay.returncode, 0, replay.stderr)
        self.assertIn("editor.ini", replay.stdout)
        self.assertNotIn("tab_size", replay.stdout)
        self.assertEqual(git("rev-parse", "HEAD"), head)
        self.assertEqual(git("status", "--porcelain"), "M editor.ini")
        self.assertEqual((self.f.config / "editor.ini").read_text(), "tab_size = 8\n")
        git("restore", "editor.ini")
        clean = self.f.dots("sync")
        self.assertEqual(clean.returncode, 0)
        self.assertEqual(len(clean.stdout.strip().splitlines()), 1, clean.stdout)
        self.assertIn("up to date", clean.stdout.lower())
        replay = self.f.dots("sync", "--last")
        self.assertEqual(replay.returncode, 0, replay.stderr)
        self.assertIn("up to date", replay.stdout.lower())
        self.assertNotIn("editor.ini", replay.stdout)

    def test_status_backend_prints_and_emits_profile_diagnosis(self) -> None:
        events = self.f.base / "events"
        mise = write(self.f.base / "bin/profiles-mise", "#!/bin/sh\necho laptop\n", 0o755)
        result = subprocess.run(["bash", "-c", 'exec 3>"$0"; exec "$@"', str(events), sys.executable,
                                 str(self.f.manager / "lib/dots-command-backend.py"), str(mise), str(self.f.config), "status"],
                                env={**self.f.env, "DOTS_STATUS_IN_ENV": "1", "DOTS_UP_EVENT_FD": "3",
                                     "SETUP_PROFILE": "unknown"}, capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stderr.splitlines(), ["setup: SETUP_PROFILE must be one of [vars] dots_profiles: laptop"])
        stages = [e for line in events.read_text().splitlines() if (e := json.loads(line))["kind"] == "stage_end"]
        self.assertEqual([(e["id"], e["detail"]) for e in stages], [("startup", result.stderr.strip())])

    def test_failed_standalone_command_names_the_command_to_rerun(self) -> None:
        import json
        events = self.f.base / "events"
        failing = write(self.f.base / "bin/failing-mise", "#!/usr/bin/env bash\necho 'no such target' >&2\nexit 1\n", 0o755)
        result = subprocess.run(["bash", "-c", 'exec 3>"$0"; exec "$@"', str(events), sys.executable,
                                 str(self.f.manager / "lib/dots-command-backend.py"), str(failing), str(self.f.config),
                                 "apply", "missing-target"],
                                cwd=self.f.config, env={**self.f.env, "DOTS_UP_EVENT_FD": "3", "DOTS_MISE_BIN": str(failing), "SETUP_PROFILE": "laptop"},
                                capture_output=True, text=True, timeout=60, check=False)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        parsed = [json.loads(line) for line in events.read_text().splitlines()]
        end = next(e for e in parsed if e["kind"] == "stage_end")
        self.assertEqual(end["status"], "failed")
        self.assertEqual(end["action"], "Review the command output and rerun dots apply missing-target.")


@requires("mise")
class ProfileTests(ContractTestCase):
    def dots(self, *args: str) -> subprocess.CompletedProcess[str]:
        return self.f.dots(*args, SETUP_MISE_BIN=mise_binary(), DOTS_MISE_BIN=mise_binary(),
                           MISE_CONFIG_DIR=str(self.f.config), MISE_AUTO_INSTALL="0")

    def test_internal_profile_selects_a_profile_of_the_configuration(self) -> None:
        write(self.f.config / "config.toml", '["vars"]\n"dots_profiles" = "laptop server"\n')
        result = self.dots("internal", "profile", "--profile", "server", "--machine-id", "fx")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Saved local setup profile server for host fx.", result.stdout)
        self.assertEqual((self.f.config / "miserc.toml").read_text(), 'env = ["server", "host-fx"]\n')
        unknown = self.dots("internal", "profile", "--profile", "desktop", "--machine-id", "fx")
        self.assertEqual(unknown.returncode, 2)
        self.assertIn("profile must be one of: laptop, server", unknown.stderr)

    def test_init_uses_the_vars_table_and_preserves_a_linked_config(self) -> None:
        config = self.f.config / "config.toml"
        config.unlink()
        target = write(self.f.base / "other/settings.toml", """
            ["vars"] # vars header
            'dots_state_repo' = "example/private" # state
            [env]
            dots_profiles = "unrelated"
            """, 0o640)
        child = target.parent / "child"
        child.mkdir()
        (self.f.config / "hop").symlink_to(child, target_is_directory=True)
        decoy = write(self.f.config / "settings.toml", '# logical traversal lands here\n')
        decoy_before = decoy.read_bytes()
        config.symlink_to("hop/../settings.toml")
        result = self.dots("init")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(decoy.read_bytes(), decoy_before)
        self.assertEqual(tomllib.loads(target.read_text()), {
            "vars": {"dots_state_repo": "example/private", "dots_profiles": "default"},
            "env": {"dots_profiles": "unrelated"},
        })
        self.assertIn('["vars"] # vars header', target.read_text())
        self.assertTrue(config.is_symlink())
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o640)
        original = target.read_bytes()
        self.assertEqual(self.dots("init").returncode, 0)
        self.assertEqual(target.read_bytes(), original)

    def test_init_keeps_quoted_profiles_and_refuses_an_invalid_candidate(self) -> None:
        config = self.f.config / "config.toml"
        for content, code in [
            (b'["vars"]\n"dots_profiles" = "laptop server"\n', 0),
            (b'vars = "cannot hold the required key"\n', 1),
        ]:
            with self.subTest(content=content):
                config.write_bytes(content)
                result = self.dots("init")
                self.assertEqual(result.returncode, code, result.stderr)
                self.assertEqual(config.read_bytes(), content)

    def test_profile_preserves_quoted_tables_comments_and_local_symlink(self) -> None:
        write(self.f.config / "config.toml", '["vars"]\n"dots_profiles" = "laptop server"\n')
        miserc = write(self.f.config / "miserc.toml", """
            "env" = [
              'laptop', # role
              'host-old',
              'extra', # extra
            ] # selectors
            ["env_sets"]
            env = ["other"] # unrelated
            """, 0o640)
        local = write(self.f.base / "local.toml", """
            ["env"] # environment
            "SETUP_PROFILE" = "server\\n" # profile comment
            KEEP = "yes" # unrelated
            """, 0o640)
        link = self.f.config / "config.local.toml"
        link.symlink_to("../local.toml")

        args = ("internal", "profile", "--profile", "server", "--machine-id", "fx")
        result = self.dots(*args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(tomllib.loads(miserc.read_text()), {
            "env": ["server", "host-fx", "extra"], "env_sets": {"env": ["other"]},
        })
        self.assertIn('# extra', miserc.read_text())
        self.assertIn('# selectors', miserc.read_text())
        self.assertIn('env = ["other"] # unrelated', miserc.read_text())
        self.assertEqual(tomllib.loads(local.read_text())["env"],
                         {"SETUP_PROFILE": "server", "SETUP_MACHINE_ID": "fx", "KEEP": "yes"})
        self.assertIn('# profile comment', local.read_text())
        self.assertIn('KEEP = "yes" # unrelated', local.read_text())
        self.assertTrue(link.is_symlink())
        self.assertEqual(stat.S_IMODE(local.stat().st_mode), 0o640)
        self.assertEqual(stat.S_IMODE(miserc.stat().st_mode), 0o640)
        original = (miserc.read_bytes(), local.read_bytes())
        self.assertEqual(self.dots(*args).returncode, 0)
        self.assertEqual((miserc.read_bytes(), local.read_bytes()), original)

    def test_profile_validation_leaves_both_files_untouched(self) -> None:
        write(self.f.config / "config.toml", '[vars]\ndots_profiles = "laptop server"\n')
        miserc = self.f.config / "miserc.toml"
        local = self.f.config / "config.local.toml"
        for selector, config in [
            ('"env" = ["laptop"]\n', '[env]\nSETUP_PROFILE = "laptop"\n[history.origin]\nbranch = "host-old"\n'),
            ('["env"]\nSETUP_PROFILE = "laptop"\n', '[env]\nSETUP_PROFILE = "laptop"\n'),
        ]:
            with self.subTest(selector=selector, config=config):
                write(miserc, selector)
                write(local, config)
                original = (miserc.read_bytes(), local.read_bytes())
                result = self.dots("internal", "profile", "--profile", "server", "--machine-id", "fx")
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertEqual((miserc.read_bytes(), local.read_bytes()), original)
        local.unlink()
        write(miserc, "# Early settings\n")
        local.symlink_to("miserc.toml")
        original = miserc.read_bytes()
        result = self.dots("internal", "profile", "--profile", "server", "--machine-id", "fx")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("must be separate files", result.stderr)
        self.assertTrue(local.is_symlink())
        self.assertEqual(miserc.read_bytes(), original)


class ConfigRootTests(ContractTestCase):
    def root(self, **env: str) -> str:
        # The manager copy sits beside the configuration, not inside it, like
        # an installed release; run from elsewhere with only ENV selecting it.
        environment = {key: value for key, value in self.f.env.items() if key != "DOTS_ROOT"}
        result = subprocess.run(["bash", "-c", f'source "{self.f.manager}/lib/setup-common.sh"; setup_init && printf "%s\\n" "$SETUP_ROOT"'],
                                cwd=self.f.base, env={**environment, "SETUP_PROFILE": "laptop", **env},
                                capture_output=True, text=True, timeout=60, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def test_library_outside_dots_finds_the_configuration_like_bin_dots(self) -> None:
        self.assertEqual(self.root(MISE_CONFIG_DIR=str(self.f.config)), str(self.f.config))
        default = self.f.home / ".config/mise"
        shutil.copytree(self.f.config, default)
        self.assertEqual(self.root(), str(default))
        self.assertEqual(self.root(DOTS_ROOT=str(self.f.config), MISE_CONFIG_DIR=str(default)), str(self.f.config))
        # bin/dots resolves callbacks by the same rule: DOTS_ROOT wins.
        listed = self.f.dots("internal", "profiles", MISE_CONFIG_DIR=str(self.f.base))
        self.assertEqual((listed.returncode, listed.stdout), (0, "laptop\nserver\n"), listed.stderr)


class HookListingTests(ContractTestCase):
    def list(self, directory: Path, profile: str = "laptop") -> subprocess.CompletedProcess[str]:
        return subprocess.run(["bash", str(self.f.manager / "lib/dots-hooks"), str(directory), profile, "laptop", "server"],
                              capture_output=True, text=True, timeout=60, check=False)

    def test_listing_labels_and_profile_suffixes(self) -> None:
        self.f.hook("bootstrap.d", "b-two", "")
        self.f.hook("bootstrap.d", "A-one.sh", "")
        self.f.hook("bootstrap.d", "-dash", "")
        self.f.hook("bootstrap.d", "skip.me~", "")
        self.f.hook("bootstrap.d", "plain", "", mode=0o644)
        self.f.hook("bootstrap.d", "10-Login-shell.sh", "")
        self.f.hook("bootstrap.d", "20_a.b.c", "")
        self.f.hook("bootstrap.d", "30-Laptop-only@laptop", "")
        self.f.hook("bootstrap.d", "31-x.sh@server", "")
        self.f.hook("bootstrap.d", "32-backup@laptop~", "")
        directory = self.f.config / "dots/hooks/bootstrap.d"
        result = self.list(directory)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            f"Login shell\t{directory}/10-Login-shell.sh", f"a.b\t{directory}/20_a.b.c",
            f"Laptop only\t{directory}/30-Laptop-only@laptop", f"A one\t{directory}/A-one.sh", f"b two\t{directory}/b-two"])
        server = self.list(directory, "server")
        self.assertEqual([line.split("\t")[0] for line in server.stdout.splitlines()],
                         ["Login shell", "a.b", "x", "A one", "b two"])
        missing = self.list(directory / "missing")
        self.assertEqual((missing.returncode, missing.stdout), (0, ""))

    def test_unknown_profile_suffix_fails_without_listing(self) -> None:
        self.f.hook("update.d", "10-fine", "")
        typo = self.f.hook("update.d", "20-typo@labtop", "")
        result = self.list(typo.parent)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "")
        self.assertIn(f'dots: hook {typo} names unknown profile "labtop"; use one of: laptop server', result.stderr)


class CommandTests(ContractTestCase):
    def command(self, name: str, body: str) -> Path:
        return write(self.f.config / "dots/commands" / name, "#!/usr/bin/env bash\n" + body, 0o755)

    def test_config_command_receives_literal_arguments_and_environment(self) -> None:
        self.command("hello", 'printf "[%s]\\n" "$@" "$DOTS_ROOT" "$DOTS_HOME" "$DOTS_PROFILE" "$PWD"\n')
        elsewhere = self.f.base / "elsewhere"
        elsewhere.mkdir()
        result = self.f.dots("hello", "two words", "--help", "*", "", "-x", cwd=elsewhere, SETUP_PROFILE="server")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ["[two words]", "[--help]", "[*]", "[]", "[-x]",
                                                      f"[{self.f.config}]", f"[{self.f.manager}]", "[server]", f"[{elsewhere}]"])
        self.assertEqual(self.f.log.read_text(), "", "a configuration command loaded mise")

    def test_builtin_commands_win(self) -> None:
        marker = self.f.base / "ran"
        self.command("sync", f"touch {marker}\n")
        result = self.f.dots("sync", "extra")
        self.assertEqual(result.returncode, 2)
        self.assertIn("dots sync [--last]", result.stderr)
        self.assertFalse(marker.exists())

    def test_names_with_a_slash_or_leading_dash_are_rejected(self) -> None:
        self.command("hello", "echo ran\n")
        for name in ("../config/dots/commands/hello", "dots/commands/hello", "-hello"):
            with self.subTest(name=name):
                result = self.f.dots(name)
                self.assertEqual(result.returncode, 2)
                self.assertIn(f"dots: invalid command name '{name}'; use dots help", result.stderr)
                self.assertNotIn("ran", result.stdout)

    def test_missing_command_is_unknown(self) -> None:
        result = self.f.dots("nope", "arg")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stderr, "dots: unknown command 'nope'; use dots help\n")
        write(self.f.config / "dots/commands/nope", "#!/usr/bin/env bash\necho ran\n", 0o644)
        self.assertEqual(self.f.dots("nope").stderr, "dots: unknown command 'nope'; use dots help\n")

    def test_internal_listings_for_the_menu(self) -> None:
        self.command("hello", "true\n")
        self.command("status", "true\n")
        write(self.f.config / "dots/commands/plain", "true\n", 0o644)
        self.assertEqual(self.f.dots("internal", "commands").stdout, "hello\n")
        self.assertEqual(self.f.dots("internal", "profiles").stdout, "laptop\nserver\n")

    def test_help_lists_configuration_commands(self) -> None:
        self.command("hello", "true\n")
        self.command("status", "true\n")
        result = self.f.dots("help")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Configuration commands", result.stdout)
        self.assertIn("  dots hello [ARGS...]", result.stdout)
        self.assertNotIn("dots status [ARGS...]", result.stdout)

    def test_renderer_discovery_skips_incompatible_candidates_but_honors_override(self) -> None:
        adjacent = write(self.f.manager / "bin/dots-up", "#!/bin/sh\necho dots-shared-cli/1\n", 0o755)
        available = write(self.f.base / "bin/dots-up", r"""
            #!/bin/sh
            if [ "$1" = --capabilities ]; then
                echo dots-shared-cli/3
            else
                printf '%s\n' "$0" "$@"
            fi
            """, 0o755)
        mise = write(self.f.base / "old-mise", f"#!/bin/sh\necho '{adjacent}'\n", 0o755)
        env = {"DOTS_MISE_BIN": str(mise), "PATH": f"{adjacent.parent}:{available.parent}:{self.f.env['PATH']}"}
        prefix = [str(available), "--root", str(self.f.config), "--home", str(self.f.manager)]
        for command, flags, args, expected in (
            ("reports", [], [], []),
            ("sync", [], [], ["--no-wait"]),
            ("add", [], ["fixture-tool"], ["--no-wait"]),
            ("add", ["--no-wait"], ["fixture-tool"], ["--no-wait"]),
        ):
            with self.subTest(command=command, flags=flags):
                result = self.f.dots(*flags, command, *args, DOTS_UP_BIN="", **env)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines(), prefix + ["--command", command, *expected, "--", *args])
        result = self.f.dots("reports", DOTS_UP_BIN=str(adjacent), **env)
        self.assertEqual(result.returncode, 2)
        self.assertIn("report browser is unavailable", result.stderr)
        self.assertEqual(result.stdout, "")


class ExceptionManifestTests(ContractTestCase):
    def read(self, manifest: str) -> subprocess.CompletedProcess[str]:
        write(self.f.config / "dots/exceptions.tsv", manifest)
        return self.f.bash(
            f'SETUP_ROOT="{self.f.config}"; SETUP_EXCEPTION_MANIFEST="$SETUP_ROOT/dots/exceptions.tsv"\n'
            'row() { printf "%s|%s|%s\\n" "$1" "${2##*/}" "$3"; }\n'
            'setup_read_exceptions row laptop || exit $?\necho ---\nsetup_read_exceptions row "" || exit $?\n'
            'echo ---\nsetup_exception_holder brew-cask:org/tap/thing@2; echo "holder=$SETUP_HOLDER"\n')

    def test_flags_column_is_optional_and_parsed(self) -> None:
        result = self.read("# comment\nlaptop\tone\nserver\ttwo\tinstall-only\nbase\tthree\tinstall-only,hold=brew-cask:thing\n\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.split("---\n"), [
            "one|one|\nthree|three|install-only,hold=brew-cask:thing\n",
            "one|one|\ntwo|two|install-only\nthree|three|install-only,hold=brew-cask:thing\n",
            "holder=three\n",
        ])

    def test_invalid_rows_stop(self) -> None:
        cases = {
            "laptop\tone\tinstall-only\textra\n": "invalid installer exception row",
            "desktop\tone\n": "invalid installer exception row",
            "laptop\tone\tsometimes\n": "unknown installer exception flag 'sometimes'",
            "laptop\tone\thold=some-app\n": "invalid hold package 'some-app'",
            "laptop\t../one\n": "invalid installer exception name",
        }
        for manifest, message in cases.items():
            with self.subTest(manifest=manifest):
                result = self.read(manifest)
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn(message, result.stderr)

    def test_missing_profiles_setting_is_named(self) -> None:
        del self.f.env["FAKE_VAR_dots_profiles"]
        result = self.read("laptop\tone\n")
        self.assertEqual(result.returncode, 2)
        self.assertIn(f"missing setting: add dots_profiles to [vars] in {self.f.config}/config.toml", result.stderr)

    def test_dots_add_refuses_a_held_package(self) -> None:
        write(self.f.config / "dots/exceptions.tsv", "laptop\tpinned\thold=brew:pinned-tool\n")
        result = self.f.dots("add", "--base", "brew:tap/x/pinned-tool@1")
        self.assertEqual(result.returncode, 2)
        self.assertIn("brew:tap/x/pinned-tool@1 is held by installer exception 'pinned'", result.stderr)


@requires("age-keygen", "jq", "mise")
class EnrollHistoryTests(ContractTestCase):
    """enroll-history keeps the host's keys and secures its declared track entries."""

    def enroll(self, *paths: str, variant: str = "host-fx", encrypt: bool = True) -> subprocess.CompletedProcess[str]:
        # bootstrap_mise passes --quiet --cd DIR --env ENV; the fake drops them.
        fake = write(self.f.base / "bin/quiet-mise", FAKE_MISE.replace(
            "    ls) printf '{}\\n' ;;",
            "    --quiet) shift; while [[ \"${1:-}\" == --cd || \"${1:-}\" == --env ]]; do shift 2; done; exec \"$0\" \"$@\" ;;\n"
            "    ls) printf '{}\\n' ;;"), 0o755)
        key = self.f.base / "keys/key.txt"
        key.parent.mkdir(exist_ok=True)
        if not key.exists():
            subprocess.run(["age-keygen", "-o", str(key)], capture_output=True, check=True)
        return subprocess.run(
            ["bash", str(self.f.manager / "bootstrap/enroll-history"), "--profile", "laptop", "--machine-id", "fx",
             "--identity", str(key), "--recipient", "age1peer"],
            cwd=self.f.config, env={**self.f.env, "MISE_CONFIG_DIR": str(self.f.config), "SETUP_MISE_BIN": str(fake),
                                    "FIXTURE_TOML_MISE": mise_binary(),
                                    "FAKE_PATHS": history_paths(self.f.home, *paths, variant=variant, encrypt=encrypt)},
            capture_output=True, text=True, timeout=60, check=False)

    def test_secures_declared_files_and_keeps_the_recipients(self) -> None:
        tracked = write(self.f.home / ".config/app/state", "state\n", 0o644)
        result = self.enroll("~/.config/app/state", "~/.config/app/missing")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(stat.S_IMODE(tracked.stat().st_mode), 0o600)
        self.assertIn("Secured 1 tracked history files", result.stdout)
        log = self.f.log.read_text()
        self.assertIn("--append --type list --file", log)
        self.assertIn("history.encryption.recipients age1peer", log)
        self.assertFalse((self.f.config / "config.host-fx.toml").exists())

    def test_a_host_without_declared_files_is_deferred(self) -> None:
        result = self.enroll()
        self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
        self.assertIn('add { profile = "host-fx" }', result.stdout)

    def test_an_unencrypted_effective_entry_stops_before_anything_is_prepared(self) -> None:
        tracked = write(self.f.home / ".config/app/state", "state\n", 0o644)
        result = self.enroll("~/.config/app/state", encrypt=False)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("history entry ~/.config/app/state in ~/.config/mise/config.host-fx.toml is not encrypted",
                      result.stderr)
        self.assertNotIn("Prepared", result.stdout)
        self.assertEqual(stat.S_IMODE(tracked.stat().st_mode), 0o644)

    def test_an_entry_for_another_host_stops(self) -> None:
        write(self.f.home / ".config/app/state", "state\n", 0o644)
        result = self.enroll("~/.config/app/state", variant="host-other")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn('does not use variants = [{ profile = "host-fx" }]', result.stderr)
        self.assertNotIn("Prepared", result.stdout)

    def test_a_symlinked_tracked_path_stops(self) -> None:
        target = write(self.f.base / "outside", "outside\n", 0o644)
        (self.f.home / ".config").mkdir(exist_ok=True)
        (self.f.home / ".config/link").symlink_to(target)
        result = self.enroll("~/.config/link")
        self.assertEqual(result.returncode, 2)
        self.assertIn("refusing symlink history path", result.stderr)
        self.assertEqual(stat.S_IMODE(target.stat().st_mode), 0o644)


@requires("jq")
class HistorySaveGuardTests(ContractTestCase):
    """A save never runs while a tracked file is declared without encryption."""

    def test_backup_refuses_an_unencrypted_entry_before_saving(self) -> None:
        result = subprocess.run(["bash", str(self.f.manager / "tasks/backup")], cwd=self.f.config,
                                env={**self.f.env, "SETUP_PROFILE": "laptop", "SETUP_MACHINE_ID": "fx",
                                     "FAKE_SETTING": "manual", "FAKE_VAR_dots_state_repo": "example/private",
                                     "FAKE_PATHS": history_paths(self.f.home, "~/.config/app/state", encrypt=False)},
                                capture_output=True, text=True, timeout=60, check=False)
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("history entry ~/.config/app/state in ~/.config/mise/config.host-fx.toml is not encrypted",
                      result.stderr)
        self.assertNotIn("dotfiles save", self.f.log.read_text())

    def test_backup_checks_effective_push_urls_and_branch_before_saving(self) -> None:
        store = self.f.base / "history/repo.git"
        other = self.f.base / "public.git"
        git_config = write(self.f.base / "gitconfig", "")
        env = {**self.f.env, "SETUP_PROFILE": "laptop", "SETUP_MACHINE_ID": "fx",
               "FAKE_SETTING": "manual", "FAKE_VAR_dots_state_repo": "example/private",
               "GIT_CONFIG_GLOBAL": str(git_config), "GIT_CONFIG_NOSYSTEM": "1"}
        for repo in (store, other):
            subprocess.run(["git", "init", "-q", "--bare", str(repo)], env=env, check=True)
        https = "https://github.com/example/private.git"
        wrong = "history push destination must be the private example/private repository"
        for origin, branch, rewrite, error in [
            (https, "host-fx", (), ""),
            ("git@github.com:example/private", "host-fx", (), ""),
            ("ssh://git@github.com/example/private.git", "host-fx", (), ""),
            (str(other), "host-fx", (), wrong),
            (https, "host-override", (), "effective history origin branch must be host-fx"),
            (https, "host-fx\n", (), "effective history origin branch must be host-fx"),
            (https + "\n", "host-fx", (), "could not read the effective history origin URL"),
            (https, "host-fx", (f"url.{other}.pushInsteadOf", https), wrong),
            (https, "host-fx", (f"remote.{https}.pushurl", str(other)), wrong),
        ]:
            with self.subTest(origin=origin, branch=branch, rewrite=rewrite):
                worked = not error
                self.f.log.write_text("")
                git_config.write_text("")
                if rewrite:
                    subprocess.run(["git", "config", "--file", str(git_config), *rewrite], env=env, check=True)
                status = {"history": {"sync": {"origin": origin, "branch": branch}, "history_repo": str(store)}}
                result = self.f.dots("backup", **env, FAKE_HISTORY_STATUS=json.dumps(status))
                self.assertEqual(result.returncode, 0 if worked else 2, result.stdout + result.stderr)
                self.assertIn(error, result.stderr)
                log = self.f.log.read_text()
                self.assertEqual("dotfiles save" in log, worked)
                self.assertEqual("dotfiles sync" in log, worked)

    def test_update_refuses_an_unencrypted_entry_before_its_checkpoint(self) -> None:
        result = self.f.update(FAKE_PATHS=history_paths(self.f.home, "~/.config/app/state", variant="", encrypt=False))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("pre-update settings checkpoint failed; no updates were started", result.stderr)
        self.assertNotIn("dotfiles save", self.f.log.read_text())

    def test_update_saves_encrypted_entries(self) -> None:
        # An unscoped entry has an empty variant field; it must not shift the declaring file.
        result = self.f.update(FAKE_PATHS=history_paths(self.f.home, "~/.config/app/state", variant=""))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.f.log.read_text().count("bootstrap dotfiles save"), 2)

    def guard(self, inventory: str, host: str = "") -> subprocess.CompletedProcess[str]:
        return self.f.bash(f'run() {{ "$SETUP_MISE_BIN" "$@"; }}; setup_tracked_history_paths run {host}',
                           FAKE_PATHS=inventory)

    def test_an_absolute_declaration_reported_under_home_is_found(self) -> None:
        inventory = history_paths(self.f.home, "~/.config/app/state", absolute=True)
        result = self.guard(inventory, "host-fx")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"{self.f.home}/.config/app/state\n")
        history_paths(self.f.home, "~/.config/app/state", absolute=True, encrypt=False)
        self.assertEqual(self.guard(inventory, "host-fx").returncode, 2)

    def test_a_missing_inventory_stops(self) -> None:
        for inventory in ("", "{}", '{"entries": {}}', '{"entries": []}\n{"entries": []}'):
            with self.subTest(inventory=inventory):
                result = self.guard(inventory)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("could not read the tracked history entries", result.stderr)
        empty = self.guard('{"entries": []}')
        self.assertEqual((empty.returncode, empty.stdout), (0, ""), empty.stderr)


@requires("jq")
class MinimalConfigurationTests(ContractTestCase):
    """A configuration with only config.toml (and its profiles) works."""

    def test_update_help_commands_and_exceptions_need_no_dots_directory(self) -> None:
        self.assertFalse((self.f.config / "dots").exists())
        result = self.f.update()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Update report: completed; no updater failures.", result.stdout)
        self.assertNotIn("installer exception", result.stdout)
        help_text = self.f.dots("help")
        self.assertEqual(help_text.returncode, 0, help_text.stderr)
        self.assertNotIn("Configuration commands", help_text.stdout)
        self.assertEqual(self.f.dots("anything").stderr, "dots: unknown command 'anything'; use dots help\n")
        listed = self.f.bash(f'SETUP_ROOT="{self.f.config}"; SETUP_EXCEPTION_MANIFEST="$SETUP_ROOT/dots/exceptions.tsv"; '
                             f'DOTS_HOME="{self.f.manager}"; setup_print_exceptions && setup_list_hooks "$SETUP_ROOT/dots/hooks/update.d"')
        self.assertEqual(listed.returncode, 0, listed.stderr)
        self.assertEqual(listed.stdout, "Installer exceptions: none declared\n")

    def test_update_names_a_missing_profiles_setting(self) -> None:
        del self.f.env["FAKE_VAR_dots_profiles"]
        result = self.f.update()
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing setting: add dots_profiles to [vars]", result.stderr)


@requires("mise")
class InitTests(ContractTestCase):
    def init(self, *args: str) -> subprocess.CompletedProcess[str]:
        environment = {key: value for key, value in self.f.env.items() if key != "DOTS_ROOT"}
        environment.update(SETUP_MISE_BIN=mise_binary(), DOTS_MISE_BIN=mise_binary(), MISE_AUTO_INSTALL="0")
        return subprocess.run([str(self.f.manager / "bin/dots"), "init", *args], cwd=self.f.base,
                              env=environment,
                              capture_output=True, text=True, timeout=60, check=False)

    def test_creates_a_working_minimal_configuration_and_is_idempotent(self) -> None:
        result = self.init("fresh")
        self.assertEqual(result.returncode, 0, result.stderr)
        fresh = self.f.base / "fresh"
        self.assertTrue((fresh / "config.toml").read_text().startswith('[vars]\n'))
        self.assertIn('dots_profiles = "default"\n', (fresh / "config.toml").read_text())
        for directory in ("hooks/update.d", "hooks/bootstrap.d", "commands", "exceptions"):
            self.assertTrue((fresh / "dots" / directory).is_dir(), directory)
        for name in ("README.md", "exceptions.tsv", "retired-dotfiles", "retired-declarations"):
            self.assertEqual((fresh / "dots" / name).read_text(), (self.f.manager / "init" / name).read_text())
        self.assertEqual((fresh / ".gitleaks.toml").read_text(), (self.f.manager / "init/gitleaks.toml").read_text())
        self.assertEqual((fresh / ".gitignore").read_text(), (self.f.manager / "init/gitignore").read_text())
        probe = self.f.base / "probe"
        subprocess.run(["git", "init", "-q", str(probe)], check=True)
        shutil.copy(fresh / ".gitignore", probe / ".gitignore")
        for local in ("miserc.toml", "config.local.toml", "config.host-fx.toml", "mise.lock", "mise.laptop.lock"):
            ignored = subprocess.run(["git", "check-ignore", "--no-index", "-q", local], cwd=probe, check=False)
            self.assertEqual(ignored.returncode, 0, f"{local} is not ignored")
        self.assertIn("created  dots/README.md", result.stdout)
        # The new configuration works: its profile is valid and its manifest parses.
        listed = self.f.dots("internal", "profiles", DOTS_ROOT=str(fresh), FAKE_VAR_dots_profiles="default")
        self.assertEqual(listed.stdout, "default\n")
        (fresh / "dots/README.md").write_text("mine\n")
        again = self.init("fresh")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertNotIn("created", again.stdout)
        self.assertIn("kept     config.toml [vars] dots_profiles", again.stdout)
        self.assertEqual((fresh / "dots/README.md").read_text(), "mine\n")

    def test_adds_settings_to_an_existing_vars_table_once(self) -> None:
        config = write(self.f.base / "existing/config.toml",
                       'min_version = "1"\n\n[vars]\nname = "x"\ndots_source_repo = "a/b"\n\n[tools]\njq = "1"\n', 0o600)
        result = self.init(str(config.parent))
        self.assertEqual(result.returncode, 0, result.stderr)
        text = config.read_text()
        self.assertEqual(text.splitlines().count("[vars]"), 1)
        self.assertLess(text.index("[vars]"), text.index('dots_profiles = "default"'))
        self.assertLess(text.index('dots_profiles = "default"'), text.index("[tools]"))
        self.assertNotIn("# dots_source_repo", text)
        self.assertIn("# Copy these optional settings into [vars].", text)
        self.assertIn("# dots_state_repo", text)
        self.assertEqual(stat.S_IMODE(config.stat().st_mode), 0o600)
        no_vars = write(self.f.base / "plain/config.toml", '[tools]\njq = "1"\n')
        self.init(str(no_vars.parent))
        self.assertTrue(no_vars.read_text().startswith('[tools]\njq = "1"\n\n[vars]\n'))


if __name__ == "__main__":
    unittest.main()
