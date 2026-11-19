#[path = "p07/isolation.rs"]
mod isolation;
mod support;

#[path = "p07/pty.rs"]
mod pty;

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;

use support::FixtureCommand;

use isolation::{IsolatedWorld, executable_on_path, fixture_file};
use pty::Pty;

const REPOSITORY: &str = "example/p07-fixture";
const GITHUB_ORIGIN: &str = "https://github.com/example/p07-fixture.git";
const PR_URL: &str = "https://github.com/example/p07-fixture/pull/17";
const MODEL: &str = "Fixture Model";
const PDF: &[u8] = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n1 0 obj\n<<>>\nendobj\n%%EOF\n";
const PDF_POLICY: &str = r#"
[[rules]]
id = "p07-fixture-marker"
description = "Synthetic fixture marker"
regex = 'P07_OUTGOING_MARKER_[0-9]{4}'
"#;

struct Publication {
    world: IsolatedWorld,
    bare: PathBuf,
    workspace: PathBuf,
    tools: PathBuf,
    log: PathBuf,
    fixture_reader: PathBuf,
    real_git: PathBuf,
    real_jj: PathBuf,
    real_gitleaks: Option<PathBuf>,
}

impl Publication {
    fn new(on_behalf_of: Option<&str>, baseline_secret: bool) -> Self {
        Self::with_seed(on_behalf_of, baseline_secret, &[])
    }

    fn with_seed(
        on_behalf_of: Option<&str>,
        baseline_secret: bool,
        files: &[(&str, &[u8])],
    ) -> Self {
        let world = IsolatedWorld::new();
        let bare = world.bare_remote("publish");
        assert!(bare.is_absolute() && bare.starts_with(world.root().parent().unwrap()));
        let tools = world.root().parent().unwrap().join("publish-tools");
        let log = world.root().parent().unwrap().join("publish-log");
        fs::create_dir(&tools).unwrap();
        fs::create_dir(&log).unwrap();
        fs::create_dir(log.join("tmp")).unwrap();
        fs::create_dir(log.join("mise-system")).unwrap();
        fs::create_dir(log.join("scan-bin")).unwrap();
        for name in [
            "bash", "cat", "dirname", "mise", "node", "python3", "readlink",
        ] {
            if let Some(tool) = executable_on_path(&world.dots(), name) {
                let tool = if name == "mise" {
                    world.native_mise_adapter(&tool)
                } else {
                    tool
                };
                symlink(tool, tools.join(name)).unwrap();
            }
        }

        let real_git =
            executable_on_path(&world.dots(), "git").expect("publish scenarios require Git");
        symlink(&real_git, log.join("scan-bin/git")).unwrap();
        let real_jj =
            executable_on_path(&world.dots(), "jj").expect("publish scenarios require jj");
        let real_gitleaks = executable_on_path(&world.dots(), "gitleaks");
        let fixture_reader = world.fixture_reader();
        let config = match on_behalf_of {
            Some(person) => format!(
                "[vars]\ndots_profiles = \"laptop\"\ndots_source_repo = \"{REPOSITORY}\"\ndots_publish_on_behalf_of = \"{person}\"\n"
            ),
            None => {
                format!("[vars]\ndots_profiles = \"laptop\"\ndots_source_repo = \"{REPOSITORY}\"\n")
            }
        };
        fs::write(world.root().join("config.toml"), config).unwrap();
        success(world.git().args(["init", "--initial-branch=main"]));
        success(world.git().args(["add", "config.toml"]));
        if baseline_secret {
            write_token(world.root());
            success(world.git().args(["add", "synthetic-token.txt"]));
        }
        for (name, bytes) in files {
            let path = world.root().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
            success(world.git().args(["add", "--", name]));
        }
        success(world.git().args(["commit", "-m", "Initial setup"]));
        success(
            world
                .git()
                .args(["push"])
                .env("GIT_ALLOW_PROTOCOL", "file")
                .arg(&bare)
                .arg("main"),
        );
        success(world.git().args(["remote", "add", "origin", GITHUB_ORIGIN]));

        let workspace = world.root().parent().unwrap().join("publish-workspace");
        success(
            world
                .git()
                .args(["clone", "--branch", "main"])
                .env("GIT_ALLOW_PROTOCOL", "file")
                .arg(&bare)
                .arg(&workspace),
        );
        let fixture = Self {
            world,
            bare,
            workspace,
            tools,
            log,
            fixture_reader,
            real_git,
            real_jj,
            real_gitleaks,
        };
        fixture.git(&["remote", "set-url", "origin", GITHUB_ORIGIN]);
        assert_eq!(
            fixture.git(&["remote", "get-url", "origin"]).trim(),
            GITHUB_ORIGIN
        );
        assert_eq!(
            fixture
                .git(&["remote", "get-url", "--push", "--all", "origin"])
                .trim(),
            GITHUB_ORIGIN
        );
        fixture.jj(&["git", "init", "--colocate"]);
        // Seed tracking metadata before running the production fetch through the local transport.
        fixture.jj(&["bookmark", "track", "main@origin"]);
        fixture.jj(&["new", "main@origin"]);
        fixture.install_shims();
        fixture
    }

    fn install_shims(&self) {
        self.shim(
            "git",
            r#"case " $* " in
    *' ls-remote --get-url '*) ;;
    *) for word do
        case "$word" in fetch|push|ls-remote|pull|clone|submodule)
            printf '%s\n' "$word" >> "$PUBLISH_LOG/git-transport"
            break ;;
        esac
    done ;;
esac
if [ "${1-}" = -C ]; then
    cd "$2"
    case "$(pwd -P)" in "$PUBLISH_AREA"|"$PUBLISH_AREA"/*) ;; *) exit 88 ;; esac
    shift 2
fi
if [ "${1-}" = --no-pager ]; then shift; fi
no_mirror=
private_remote=
if [ "${1-}" = -c ] && [ "${2-}" = remote.origin.mirror=false ]; then
    no_mirror=1
    shift 2
elif [ "${1-}" = -c ]; then
    rewrite=$2
    case "$rewrite" in
        "url.$PUBLISH_GITHUB.insteadOf=https://dots-publish.invalid/dots-publish-"*) ;;
        *) exit 88 ;;
    esac
    private_remote=${rewrite##*/}
    case "$private_remote" in *[!a-zA-Z0-9_-]*) exit 88 ;; esac
    alias="https://dots-publish.invalid/$private_remote"
    [ "${3-}" = -c ] && [ "${4-}" = "remote.$private_remote.url=$alias" ]
    [ "${5-}" = -c ] && [ "${6-}" = "remote.$private_remote.pushurl=$alias" ]
    url_config=$4
    push_config=$6
    shift 6
    case "${1-} ${2-} ${3-}" in
        "ls-remote --get-url $private_remote")
            [ "$#" -eq 3 ]
            exec "$PUBLISH_GIT" -c "$rewrite" -c "$url_config" -c "$push_config" "$@"
            ;;
        push\ *|"ls-remote --exit-code --refs") ;;
        *) exit 88 ;;
    esac
fi
case "${1-} ${2-}" in
    'remote '|'remote -v'|'remote get-url'|'remote set-url') exec "$PUBLISH_GIT" "$@" ;;
esac
case "${1-}" in
    fetch|push|ls-remote)
        export GIT_ALLOW_PROTOCOL=file
        [ "$("$PUBLISH_GIT" rev-parse --show-toplevel)" = "$PUBLISH_WORKSPACE" ]
        [ "$("$PUBLISH_GIT" remote)" = origin ]
        [ -d "$PUBLISH_BARE/objects" ] && [ -f "$PUBLISH_BARE/HEAD" ]
        [ "$("$PUBLISH_GIT" remote get-url origin)" = "$PUBLISH_GITHUB" ]
        [ "$("$PUBLISH_GIT" remote get-url --push --all origin)" = "$PUBLISH_GITHUB" ]
        target=${private_remote:-origin}
        first=1
        destination=
        for arg do
            if [ "$first" = 1 ]; then first=0; continue; fi
            if [ -z "$destination" ]; then
                case "$arg" in
                    "$target") destination=fixture; continue ;;
                    --repo*|--receive-pack*|--upload-pack*|--all|--exec*) exit 88 ;;
                    -*) ;;
                    *) exit 88 ;;
                esac
            fi
            case "$arg" in
                --repo*|--all|--receive-pack*|--upload-pack*|--exec*|/*|*://*) exit 88 ;;
            esac
            if [ "$1" = push ]; then
                case "$arg" in +*|--force*|-f) exit 88 ;; esac
            fi
        done
        [ "$destination" = fixture ]
        if [ "$no_mirror" = 1 ]; then set -- -c remote.origin.mirror=false "$@"; fi
        if [ -n "$private_remote" ]; then
            # Verify the genuine command-local mapping before permitting a
            # pinned push or subsequent ref lookup in the private bare fixture.
            [ "$("$PUBLISH_GIT" -c "$rewrite" -c "$url_config" -c "$push_config" ls-remote --get-url "$private_remote")" = "$PUBLISH_GITHUB" ]
            exec "$PUBLISH_GIT" -c "remote.$private_remote.url=$PUBLISH_BARE" -c "remote.$private_remote.pushurl=$PUBLISH_BARE" "$@"
        fi
        exec "$PUBLISH_GIT" -c "url.$PUBLISH_BARE.insteadOf=$PUBLISH_GITHUB" "$@"
        ;;
    clone|pull|submodule|remote|-c|--config-env|--git-dir*|--work-tree*) exit 88 ;;
    *) exec "$PUBLISH_GIT" "$@" ;;
esac
"#,
        );
        self.shim(
            "jj",
            r#"printf '%s\n' "$*" >> "$PUBLISH_LOG/jj"
if [ "${PUBLISH_NO_DETACHED:-}" = 1 ]; then
    for arg do
        if [ "$arg" = --no-integrate-operation ]; then
            printf 'error: unexpected argument %s\n' "$arg" >&2
            exit 2
        fi
    done
fi
classify() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --no-pager|--color=never|--ignore-working-copy|--no-integrate-operation|--at-op=*) shift ;;
            --at-op) shift 2 ;;
            --config=ui.quiet=false|--config=debug.randomness-seed=*|--config=debug.commit-timestamp=*|'--config=revset-aliases."immutable_heads()"='*) shift ;;
            "--config=templates.file_show='\"\"'")
                [ "${2-} ${3-}" = 'file show' ] || return 88
                shift ;;
            --config)
                case "$2" in git.abandon-unreachable-commits=false|debug.commit-timestamp=*) ;; *) return 88 ;; esac
                shift 2 ;;
            --repository|-R) [ "$2" = "$PUBLISH_WORKSPACE" ]; shift 2 ;;
            *) break ;;
        esac
    done
    case "${1-} ${2-}" in
        'git fetch'|'git push') printf transport; return ;;
        'git root') printf local; return ;;
        'config get') [ "$#" -eq 3 ] && [ "$3" = 'revset-aliases."immutable_heads()"' ]; printf local; return ;;
        'workspace update-stale') [ "$#" -eq 2 ]; printf local; return ;;
        'resolve --list')
            [ "$#" -eq 6 ] && [ "$3" = -r ] && [ "$5" = -- ]
            case "$4" in commit_id\(*\)) ;; *) return 88 ;; esac
            printf local; return ;;
    esac
    case "${1-}" in
        bookmark|describe|diff|edit|file|log|new|op|operation|root|show|status|--version|version) printf local ;;
        *) return 88 ;;
    esac
}
mode=$(classify "$@")
case "$mode" in
    transport)
        for arg do
            case "$arg" in --force*|-f) exit 88 ;; esac
        done
        # The production Git validation sees synthetic GitHub URLs. Only this JJ
        # transport invocation changes origin, and only to the fixture's bare repo.
        [ -d "$PUBLISH_BARE/objects" ]
        [ -f "$PUBLISH_BARE/HEAD" ]
        [ "$(git rev-parse --show-toplevel)" = "$PUBLISH_WORKSPACE" ]
        cd "$PUBLISH_WORKSPACE"
        [ "$(git remote)" = origin ]
        [ "$(git remote get-url origin)" = "$PUBLISH_GITHUB" ]
        [ "$(git remote get-url --push --all origin)" = "$PUBLISH_GITHUB" ]
        git remote set-url origin "$PUBLISH_BARE"
        status=0
        PATH="$PUBLISH_LOG/scan-bin:$PATH" GIT_ALLOW_PROTOCOL=file "$PUBLISH_JJ" "$@" || status=$?
        git remote set-url origin "$PUBLISH_GITHUB"
        exit "$status"
        ;;
    local)
        if [ "${PUBLISH_INTERRUPT_ON_INTEGRATE:-0}" = 1 ]; then
            case " $* " in
                *' operation integrate '*)
                    "$PUBLISH_JJ" "$@"
                    : > "$PUBLISH_LOG/integrated"
                    kill -INT "$PPID"
                    exit 0 ;;
            esac
        fi
        exec "$PUBLISH_JJ" "$@" ;;
    *) exit 88 ;;
esac
"#,
        );
        self.shim(
            "gh",
            r#"printf '%s\n' "$*" >> "$PUBLISH_LOG/gh"
case "${1-} ${2-}" in
    'pr list')
        case " $* " in
            *' --head wip/dotfiles-all-dirty '*)
                if [ -f "$PUBLISH_LOG/existing-pr.json" ]; then
                    "$PUBLISH_READ_FILE" "$PUBLISH_LOG/existing-pr.json"; exit 0
                fi ;;
        esac
        printf '%s\n' '[]' ;;
    'pr create'|'pr edit')
        shift 2
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --body-file|-F)
                    if [ "$2" = - ]; then
                        cat > "$PUBLISH_LOG/body"
                    else
                        "$PUBLISH_READ_FILE" "$2" > "$PUBLISH_LOG/body"
                    fi
                    shift 2 ;;
                --body|-b)
                    printf '%s\n' "$2" > "$PUBLISH_LOG/body"
                    shift 2 ;;
                *) shift ;;
            esac
        done
        [ -f "$PUBLISH_LOG/body" ]
        printf '%s\n' "$PUBLISH_PR_URL"
        ;;
    --version\ ) printf '%s\n' 'gh version 2.0.0' ;;
    *) exit 89 ;;
esac
"#,
        );
        self.shim(
            "gitleaks",
            r#"if [ "${1-}" = version ] || [ "${1-}" = --version ]; then
    printf '%s\n' '8.30.1'
    exit 0
fi
cwd=$(pwd -P)
for operand do :; done
case "$operand" in
    /*) source=$operand ;;
    *) source="$cwd/$operand" ;;
esac
case "$source" in "$PUBLISH_AREA"/*) ;; *) exit 88 ;; esac
case "$source/" in */../*) exit 88 ;; esac
[ ! -L "$source" ]
printf '%s\t%s\t%s\n' "$1" "$cwd" "$operand" >> "$PUBLISH_LOG/scan-location"
case "$1" in
    git)
        [ "$cwd" = "$PUBLISH_WORKSPACE" ]
        source=$(cd "$source" && pwd -P)
        [ "$source" = "$cwd" ]
        range=
        for arg do
            case "$arg" in
                --log-opts=*)
                    case "$arg" in *--diff-merges=first-parent*) ;; *) exit 88 ;; esac
                    range=${arg##* } ;;
            esac
        done
        case "$range" in *[!a-f0-9.]*|'') exit 88 ;; esac
        if [ -n "${PUBLISH_SCAN_BASE:-}" ]; then
            [ "$range" = "$PUBLISH_SCAN_BASE..$PUBLISH_SCAN_HEAD" ]
        fi
        "$PUBLISH_GIT" -C "$source" log --format= --name-only --diff-merges=first-parent "$range" -- >> "$PUBLISH_LOG/scan-input"
        ;;
    dir)
        case "$cwd" in "$PUBLISH_LOG"/tmp/*) ;; *) exit 88 ;; esac
        if [ -d "$source" ]; then
            source=$(cd "$source" && pwd -P)
            case "$source" in "$PUBLISH_LOG"/tmp/*) ;; *) exit 88 ;; esac
            for path in "$source"/public-*.txt "$source"/synthetic-token.txt "$source"/payload.zip "$source"/opaque.bin "$source"/binary.dat "$source"/*.pdf "$source"/collision.txt; do
                if [ -f "$path" ] && [ ! -L "$path" ]; then
                    name=${path##*/}
                    printf '%s\n' "$name" >> "$PUBLISH_LOG/scan-input"
                    if [ -n "${PUBLISH_SCAN_HEAD:-}" ]; then
                        "$PUBLISH_GIT" -C "$PUBLISH_WORKSPACE" cat-file blob "$PUBLISH_SCAN_HEAD:$name" > "$PUBLISH_LOG/approved-bytes"
                        if python3 -I -S -c '
import sys
from pathlib import Path

scanned, approved = map(Path, sys.argv[1:])
sys.exit(scanned.read_bytes() != approved.read_bytes())
' "$path" "$PUBLISH_LOG/approved-bytes"; then
                            printf '%s\n' "$name" >> "$PUBLISH_LOG/scan-approved-bytes"
                        fi
                    fi
                fi
            done
        elif [ -f "$source" ]; then
            parent=$(cd "${source%/*}" && pwd -P)
            case "$parent" in "$PUBLISH_LOG"/tmp/*) ;; *) exit 88 ;; esac
            printf '%s\n' "${source##*/}" >> "$PUBLISH_LOG/scan-input"
        else
            exit 88
        fi
        ;;
    *) exit 88 ;;
esac
status=0
if [ "${PUBLISH_REAL_SCAN-}" = 1 ]; then
    command=$1
    shift
    PATH="$PUBLISH_LOG/scan-bin" "$PUBLISH_GITLEAKS" "$command" \
        --report-format json --report-path "$PUBLISH_LOG/findings.json" "$@" \
        2> "$PUBLISH_LOG/scan-stderr" || status=$?
    cat "$PUBLISH_LOG/scan-stderr" >&2
    printf '%s\n' "$status" > "$PUBLISH_LOG/scan-status"
fi
: > "$PUBLISH_LOG/scan-complete"
exit "$status"
"#,
        );
    }

    fn shim(&self, name: &str, source: &str) {
        let path = self.tools.join(name);
        fs::write(&path, format!("#!/bin/sh\nset -eu\n{source}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(&self.workspace, args)
    }

    fn git_in(&self, cwd: &Path, args: &[&str]) -> String {
        success(
            self.world
                .git()
                .args(args)
                .current_dir(cwd)
                .env("GIT_ALLOW_PROTOCOL", "file"),
        )
    }

    fn jj(&self, args: &[&str]) -> String {
        success(
            self.world
                .tool(&self.real_jj)
                .current_dir(&self.workspace)
                .env("GIT_ALLOW_PROTOCOL", "file")
                .args(args),
        )
    }

    fn change(&self, index: usize) {
        fs::write(
            self.workspace.join(format!("public-{index}.txt")),
            "fixture\n",
        )
        .unwrap();
        self.jj(&["describe", "-m", &format!("Publish fixture change {index}")]);
    }

    fn change_main(&self, path: &str, bytes: &[u8]) -> String {
        let root = self.world.root();
        fs::write(root.join(path), bytes).unwrap();
        self.git_in(root, &["add", "--", path]);
        self.git_in(root, &["commit", "-m", &format!("Change {path} on main")]);
        self.git_in(root, &["push", self.bare.to_str().unwrap(), "main"]);
        self.git_in(root, &["rev-parse", "main"]).trim().to_owned()
    }

    fn observe_jj(&self, args: &[&str]) -> String {
        self.jj(&[&["--at-op=@", "--ignore-working-copy"][..], args].concat())
    }

    fn working_oid(&self) -> String {
        self.observe_jj(&["log", "--no-graph", "-r", "@", "-T", "commit_id"])
            .trim()
            .to_owned()
    }

    fn local_bookmarks(&self) -> String {
        self.observe_jj(&[
            "bookmark",
            "list",
            "-T",
            "if(!remote, name ++ \" \" ++ normal_target.commit_id() ++ \"\\n\")",
        ])
    }

    fn scan_all(&self) -> FixtureCommand<'_> {
        let mut command = self.dots(&["publish", "--all"]);
        self.real_scan(&mut command);
        command
    }

    fn dots(&self, args: &[&str]) -> FixtureCommand<'_> {
        let mut command = self.world.dots();
        command
            .current_dir(&self.workspace)
            .args(args)
            .env("PATH", &self.tools)
            .env("TMPDIR", self.log.join("tmp"))
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_EXEC_AUTO_INSTALL", "0")
            .env("MISE_AUTO_UPDATE", "0")
            .env("MISE_NOT_FOUND_AUTO_INSTALL", "0")
            .env("MISE_OFFLINE", "1")
            .env("MISE_SYSTEM_CONFIG_DIR", self.log.join("mise-system"))
            .env("MISE_CEILING_PATHS", self.world.root().parent().unwrap())
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("PUBLISH_LOG", &self.log)
            .env("PUBLISH_AREA", self.world.root().parent().unwrap())
            .env("PUBLISH_BARE", &self.bare)
            .env("PUBLISH_GITHUB", GITHUB_ORIGIN)
            .env("PUBLISH_WORKSPACE", &self.workspace)
            .env("PUBLISH_GIT", &self.real_git)
            .env("PUBLISH_JJ", &self.real_jj)
            .env("PUBLISH_PR_URL", PR_URL)
            .env("PUBLISH_READ_FILE", &self.fixture_reader)
            .env_remove("DOTS_PUBLISH_MODEL");
        if let Some(gitleaks) = &self.real_gitleaks {
            command.env("PUBLISH_GITLEAKS", gitleaks);
        }
        command
    }

    fn real_scan(&self, command: &mut FixtureCommand<'_>) {
        assert!(
            self.real_gitleaks.is_some(),
            "real-scanner scenario requires gitleaks"
        );
        command.env("PUBLISH_REAL_SCAN", "1");
    }

    fn read_log(&self, name: &str) -> String {
        fs::read_to_string(self.log.join(name))
            .unwrap_or_else(|error| panic!("read fixture log {name}: {error}"))
    }

    fn optional_log(&self, name: &str) -> String {
        match fs::read_to_string(self.log.join(name)) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => panic!("read fixture log {name}: {error}"),
        }
    }

    fn deny_tool_probes(&self) {
        for tool in ["jj", "gh", "gitleaks"] {
            self.shim(
                tool,
                "printf '%s\n' \"$0\" >> \"$PUBLISH_LOG/unexpected-tool\"\nexit 90\n",
            );
        }
    }

    fn denied_without_contact(&self, status: i32, no_probes: bool) -> String {
        let output = self.dots(&["publish"]).output().unwrap();
        assert_eq!(output.status.code(), Some(status), "{output:?}");
        let diagnostic = format!(
            "{}{}",
            String::from_utf8(output.stdout).unwrap(),
            String::from_utf8(output.stderr).unwrap()
        );
        if status == 1 {
            assert_eq!(diagnostic.lines().count(), 1, "{diagnostic}");
        } else {
            assert!(!diagnostic.trim().is_empty());
        }
        assert!(self.optional_log("git-transport").is_empty());
        assert!(
            !self
                .optional_log("jj")
                .lines()
                .any(|line| line.contains("git fetch")),
            "attempted an unvalidated fetch"
        );
        if no_probes {
            assert!(self.optional_log("unexpected-tool").is_empty());
        }
        self.assert_no_publication();
        diagnostic
    }

    fn assert_no_publication(&self) {
        assert!(
            !self
                .optional_log("git-transport")
                .lines()
                .any(|line| line == "push")
        );
        assert!(
            !self
                .optional_log("jj")
                .lines()
                .any(|line| line.contains("git push"))
        );
        assert!(
            !self
                .optional_log("gh")
                .lines()
                .any(|line| { line.starts_with("pr create ") || line.starts_with("pr edit ") })
        );
        assert!(
            success(self.world.git().args(["--git-dir"]).arg(&self.bare).args([
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/"
            ]),)
            .trim()
                == "refs/heads/main"
        );
    }

    fn approve(
        &self,
        mut command: FixtureCommand<'_>,
        revision: &str,
        expected_paths: &[&str],
    ) -> String {
        let approved = self.jj(&["log", "--no-graph", "-r", revision, "-T", "commit_id"]);
        let approved = approved.trim();
        let main = success(
            self.world
                .git()
                .arg("--git-dir")
                .arg(&self.bare)
                .args(["rev-parse", "refs/heads/main"]),
        );
        command
            .env("PUBLISH_SCAN_BASE", main.trim())
            .env("PUBLISH_SCAN_HEAD", approved);
        self.real_scan(&mut command);
        let mut terminal = Pty::spawn(command);
        terminal.expect("[y/N]");
        assert!(
            self.log.join("scan-complete").is_file(),
            "confirmation preceded scanner completion"
        );
        assert_eq!(self.read_log("scan-status"), "0\n");
        let locations = self.read_log("scan-location");
        assert!(
            !locations.is_empty(),
            "scanner must observe a cwd and source"
        );
        if locations.lines().any(|line| line.starts_with("dir\t")) {
            let bytes = self.read_log("scan-approved-bytes");
            let mut matching: Vec<_> = bytes.lines().collect();
            matching.sort_unstable();
            matching.dedup();
            assert_eq!(
                matching, expected_paths,
                "scanner missed approved file bytes"
            );
        }
        let inputs = self.read_log("scan-input");
        let mut scanned: Vec<_> = inputs.lines().filter(|line| !line.is_empty()).collect();
        scanned.sort_unstable();
        scanned.dedup();
        assert!(!scanned.is_empty());
        assert_eq!(scanned, expected_paths);
        self.assert_no_publication();
        terminal.send("y\n");
        let (status, transcript) = terminal.finish();
        assert!(status.success(), "{status}\n{transcript}");
        assert!(transcript.contains(PR_URL), "{transcript}");
        let calls = self.read_log("gh");
        let create = calls
            .lines()
            .find(|line| line.starts_with("pr create "))
            .expect("expected a PR creation request");
        let argument = |flag: &str| {
            create
                .split_ascii_whitespace()
                .skip_while(|word| *word != flag)
                .nth(1)
        };
        assert_eq!(argument("--base"), Some("main"));
        let head = argument("--head").expect("PR creation must name its published head");
        assert!(
            head.starts_with("wip/"),
            "not a private publication bookmark: {head}"
        );
        if revision != "@" {
            assert_eq!(head, revision);
        }
        let reference = format!("refs/heads/{head}");
        let refs = success(self.world.git().arg("--git-dir").arg(&self.bare).args([
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads/",
        ]));
        let pushed: Vec<_> = refs
            .lines()
            .filter_map(|line| {
                let (name, oid) = line.split_once(' ').unwrap();
                (name != "refs/heads/main").then_some((name, oid))
            })
            .collect();
        assert_eq!(pushed, [(reference.as_str(), approved)], "{refs}");
        assert!(
            refs.lines()
                .any(|line| line == format!("refs/heads/main {}", main.trim()))
        );
        approved.to_owned()
    }

    fn reject_secret(&self, bookmark: &str, terminal: bool) -> String {
        let mut command = self.dots(&["publish", "--bookmark", bookmark]);
        self.real_scan(&mut command);
        let transcript = refusal(command, terminal);
        let report = fs::read_to_string(self.log.join("findings.json")).unwrap_or_else(|error| {
            panic!(
                "scanner report unavailable: {error}\n{}",
                transcript.replace(&synthetic_token(), "[synthetic token]")
            )
        });
        assert!(!report.contains(&synthetic_token()));
        let findings: Vec<serde_json::Value> = serde_json::from_str(&report).unwrap();
        assert_eq!(
            findings
                .iter()
                .filter(|finding| {
                    finding["RuleID"] == "github-pat"
                        && finding["File"]
                            .as_str()
                            .and_then(|path| Path::new(path).file_name())
                            == Some(std::ffi::OsStr::new("synthetic-token.txt"))
                        && finding["Secret"] == "REDACTED"
                })
                .count(),
            1,
            "expected one redacted github-pat finding in the fixture file",
        );
        assert!(
            transcript.contains("gitleaks found potential secrets"),
            "publish did not report scanner findings"
        );
        assert_ne!(self.read_log("scan-status"), "0\n");
        self.assert_no_publication();
        transcript
    }
}

fn success(command: &mut FixtureCommand<'_>) -> String {
    let result = command.output().unwrap();
    assert!(result.status.success(), "{:?}\n{result:?}", **command);
    String::from_utf8(result.stdout).unwrap()
}

fn failure(output: Output) -> (String, String) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn refusal(mut command: FixtureCommand<'_>, terminal: bool) -> String {
    if terminal {
        let mut pty = Pty::spawn(command);
        pty.send("n\n");
        let (status, transcript) = pty.finish();
        assert_eq!(status.code(), Some(1), "{transcript}");
        transcript
    } else {
        let (stdout, stderr) = failure(command.output().unwrap());
        format!("{stdout}{stderr}")
    }
}

fn assert_no_preview(transcript: &str) {
    for marker in [
        "Proposed changes",
        "Proposed diff",
        "Proposed pull request title",
        "Proposed pull request description",
        "Final pull request diff",
        "diff --git",
        "@@ ",
        "[y/N]",
    ] {
        assert!(
            !transcript.contains(marker),
            "found {marker:?}: {transcript}"
        );
    }
}

fn synthetic_token() -> String {
    ["ghp", "R8tq3LwVn5Xk2Jc9Hy7Pd4Mb6Fz1Sa0Gu3Ee"].join("_")
}

fn write_token(root: &Path) {
    fs::write(
        root.join("synthetic-token.txt"),
        format!("github_token = \"{}\"\n", synthetic_token()),
    )
    .unwrap();
}

fn zipped(name: &str, contents: &[u8]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file(
        name,
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated),
    )
    .unwrap();
    zip.write_all(contents).unwrap();
    zip.finish().unwrap().into_inner()
}

#[test]
fn new_publish_finds_a_redacted_secret_one_archive_level_deep() {
    for terminal in [false, true] {
        assert_archive_finding_before_preview(terminal);
    }
}

fn assert_archive_finding_before_preview(terminal: bool) {
    let fixture = Publication::new(None, false);
    let token = synthetic_token();
    let archive = fixture.workspace.join("payload.zip");
    fs::write(
        &archive,
        zipped(
            "nested/synthetic-token.txt",
            format!("github_token = \"{token}\"\n").as_bytes(),
        ),
    )
    .unwrap();
    let original = fs::read(&archive).unwrap();
    fixture.jj(&["describe", "-m", "Add an archive"]);
    let bookmark = "wip/archive";
    fixture.jj(&["bookmark", "create", bookmark, "-r", "@"]);
    let head = fixture.jj(&["log", "--no-graph", "-r", bookmark, "-T", "commit_id"]);
    let mut command = fixture.dots(&["publish", "--bookmark", bookmark]);
    fixture.real_scan(&mut command);
    let transcript = refusal(command, terminal);
    assert!(transcript.contains("gitleaks found potential secrets"));
    assert_no_preview(&transcript);
    assert!(!transcript.contains(&token));
    assert_eq!(fixture.read_log("scan-input"), "payload.zip\n");
    let report = fixture.read_log("findings.json");
    assert!(!report.contains(&token));
    let findings: Vec<serde_json::Value> = serde_json::from_str(&report).unwrap();
    assert_eq!(
        findings
            .iter()
            .filter(|finding| {
                finding["RuleID"] == "github-pat"
                    && finding["Secret"] == "REDACTED"
                    && finding["File"].as_str().is_some_and(|path| {
                        path.contains("payload.zip") && path.ends_with("nested/synthetic-token.txt")
                    })
            })
            .count(),
        1
    );
    assert_eq!(fs::read(archive).unwrap(), original);
    assert_eq!(
        fixture.jj(&["log", "--no-graph", "-r", bookmark, "-T", "commit_id"]),
        head
    );
    fixture.assert_no_publication();
}

#[test]
fn new_publish_allows_a_clean_one_level_archive_to_reach_confirmation() {
    let fixture = Publication::new(None, false);
    fs::write(
        fixture.workspace.join("payload.zip"),
        zipped("nested/notes.txt", b"fixture notes\n"),
    )
    .unwrap();
    fixture.jj(&["describe", "-m", "Add a clean archive"]);
    let mut command = fixture.dots(&["publish"]);
    fixture.real_scan(&mut command);
    fixture.approve(command, "@", &["payload.zip"]);
}

#[test]
fn new_publish_allows_native_success_without_a_skip_signal() {
    let fixture = Publication::new(None, false);
    fs::write(fixture.workspace.join("opaque.bin"), [0, 129, 254, 7]).unwrap();
    fixture.jj(&["describe", "-m", "Add opaque binary data"]);
    let mut command = fixture.dots(&["publish"]);
    fixture.real_scan(&mut command);
    fixture.approve(command, "@", &["opaque.bin"]);
    assert_eq!(fixture.read_log("scan-status"), "0\n");
    let native_log = fixture.read_log("scan-stderr");
    assert!(!native_log.contains("exceeds max archive depth"));
    assert!(!native_log.contains("skipping binary file"));
}

#[test]
fn new_publish_refuses_explicit_native_format_and_depth_skips_before_confirmation() {
    for terminal in [false, true] {
        assert_archive_skip_before_preview(terminal);
    }
}

fn assert_archive_skip_before_preview(terminal: bool) {
    const PDF: &[u8] = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n1 0 obj\n<<>>\nendobj\n%%EOF\n";
    let nested = zipped("inner.zip", &zipped("notes.txt", b"fixture notes\n"));
    for (path, bytes, signal, field, diagnostic) in [
        (
            "payload.zip",
            nested.as_slice(),
            "skipping archive: exceeds max archive depth",
            "max_archive_depth=1",
            "one-level scan limit",
        ),
        (
            "binary.dat",
            PDF,
            "skipping binary file",
            "mime_type=application/pdf",
            "unsupported file format",
        ),
    ] {
        let fixture = Publication::new(None, false);
        let input = fixture.workspace.join(path);
        fs::write(&input, bytes).unwrap();
        fixture.jj(&["describe", "-m", "Add fixture data"]);
        let bookmark = "wip/archive";
        fixture.jj(&["bookmark", "create", bookmark, "-r", "@"]);
        let head = fixture.jj(&["log", "--no-graph", "-r", bookmark, "-T", "commit_id"]);
        let mut command = fixture.dots(&["publish", "--bookmark", bookmark]);
        fixture.real_scan(&mut command);
        let transcript = refusal(command, terminal);
        assert_no_preview(&transcript);
        assert!(transcript.contains(diagnostic), "{transcript}");
        assert!(!transcript.contains(fixture.log.to_str().unwrap()));
        assert_eq!(fixture.read_log("scan-input"), format!("{path}\n"));
        assert_eq!(fixture.read_log("scan-status"), "0\n");
        assert!(
            fixture
                .read_log("scan-stderr")
                .lines()
                .any(|line| line.contains(signal) && line.contains(field) && line.contains(path)),
            "native gitleaks did not report the expected format or depth skip"
        );
        assert_eq!(fs::read(input).unwrap(), bytes);
        assert_eq!(
            fixture.jj(&["log", "--no-graph", "-r", bookmark, "-T", "commit_id"]),
            head
        );
        fixture.assert_no_publication();
    }
}

fn pdf_allowlist() -> String {
    format!("{PDF_POLICY}\n[[allowlists]]\npaths = ['[.]pdf$']\n")
}

fn outgoing_pdf(fixture: &Publication) {
    fs::write(fixture.workspace.join("outgoing.pdf"), PDF).unwrap();
    fixture.jj(&["describe", "-m", "Add outgoing PDF"]);
    fixture.jj(&["bookmark", "create", "wip/keep", "-r", "@"]);
}

fn decline(mut terminal: Pty) -> String {
    terminal.send("n\n");
    let (status, transcript) = terminal.finish();
    assert_eq!(status.code(), Some(1), "{transcript}");
    transcript
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_diverged_all_uses_main_removed_allowlist_before_any_prompt() {
    let fixture = Publication::with_seed(
        None,
        false,
        &[(".gitleaks.toml", pdf_allowlist().as_bytes())],
    );
    outgoing_pdf(&fixture);
    let working = fixture.working_oid();
    let bookmarks = fixture.local_bookmarks();
    fixture.change_main(".gitleaks.toml", PDF_POLICY.as_bytes());

    let transcript = decline(Pty::spawn(fixture.scan_all()));
    assert!(
        transcript.contains("unsupported file format"),
        "{transcript}"
    );
    assert!(!transcript.contains("[y/N]"), "{transcript}");
    assert!(
        !transcript.contains("Proposed pull request"),
        "{transcript}"
    );
    let native = fixture.read_log("scan-stderr");
    assert!(native.lines().any(|line| {
        line.contains("skipping binary file")
            && line.contains("mime_type=application/pdf")
            && line.contains("outgoing.pdf")
    }));
    assert_eq!(fixture.working_oid(), working);
    assert_eq!(fixture.local_bookmarks(), bookmarks);
    assert_eq!(
        fs::read(fixture.workspace.join("outgoing.pdf")).unwrap(),
        PDF
    );
    fixture.assert_no_publication();
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_diverged_all_uses_main_added_allowlist_and_keeps_only_the_approved_merge() {
    let fixture = Publication::with_seed(None, false, &[(".gitleaks.toml", PDF_POLICY.as_bytes())]);
    outgoing_pdf(&fixture);
    fixture.jj(&["bookmark", "untrack", "main@origin"]);
    let working = fixture.working_oid();
    let bookmarks = fixture.local_bookmarks();
    let policy = pdf_allowlist();
    let main = fixture.change_main(".gitleaks.toml", policy.as_bytes());

    let mut first = Pty::spawn(fixture.scan_all());
    first.expect("Merge main@origin");
    first.expect("[y/N]");
    assert!(fixture.log.join("scan-complete").is_file());
    assert_eq!(fixture.working_oid(), working);
    assert_eq!(fixture.local_bookmarks(), bookmarks);
    decline(first);
    assert_eq!(fixture.working_oid(), working);
    assert_eq!(fixture.local_bookmarks(), bookmarks);
    fixture.assert_no_publication();

    fs::remove_file(fixture.log.join("scan-complete")).unwrap();
    let mut second = Pty::spawn(fixture.scan_all());
    second.expect("Merge main@origin");
    second.expect("[y/N]");
    assert!(fixture.log.join("scan-complete").is_file());
    second.send("y\n");
    second.expect("[y/N]");
    let merged = fixture.working_oid();
    assert_ne!(merged, working);
    assert_eq!(
        fixture.git(&["rev-parse", &format!("{merged}^2")]).trim(),
        main
    );
    assert_eq!(
        fixture.git(&["show", &format!("{merged}:.gitleaks.toml")]),
        policy
    );
    decline(second);
    assert_eq!(fixture.working_oid(), merged);
    assert_eq!(fixture.local_bookmarks(), bookmarks);
    fixture.assert_no_publication();

    let approved = fixture.approve(fixture.scan_all(), "@", &["outgoing.pdf"]);
    assert_eq!(approved, merged);
    assert_eq!(
        fixture
            .observe_jj(&["log", "--no-graph", "-r", "@-", "-T", "commit_id"])
            .trim(),
        approved
    );
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_aggregate_merge_does_not_scan_main_only_pdf() {
    let fixture = Publication::with_seed(None, false, &[(".gitleaks.toml", PDF_POLICY.as_bytes())]);
    fixture.change(0);
    let working = fixture.working_oid();
    fixture.change_main("main-only.pdf", PDF);
    let mut terminal = Pty::spawn(fixture.scan_all());
    terminal.expect("Merge main@origin");
    terminal.expect("[y/N]");
    let inputs = fixture.read_log("scan-input");
    assert!(inputs.lines().any(|path| path == "public-0.txt"));
    assert!(!inputs.lines().any(|path| path == "main-only.pdf"));
    decline(terminal);
    assert_eq!(fixture.working_oid(), working);
    fixture.assert_no_publication();
}

fn historical_merge() -> Publication {
    let fixture = Publication::with_seed(None, false, &[(".gitleaks.toml", PDF_POLICY.as_bytes())]);
    fixture.change(0);
    let outgoing = fixture.working_oid();
    let older_main = fixture.change_main("main-only.pdf", PDF);
    fixture.git(&[
        "fetch",
        "--no-tags",
        fixture.bare.to_str().unwrap(),
        "refs/heads/main:refs/remotes/origin/main",
    ]);
    fixture.jj(&["git", "import"]);
    fixture.jj(&["new", &outgoing, &older_main, "-m", "Previous source merge"]);
    fixture
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_historical_merge_ignores_pdf_inherited_from_older_main_parent() {
    let fixture = historical_merge();
    fixture.change_main("later.txt", b"later reviewed main\n");
    let working = fixture.working_oid();
    let mut terminal = Pty::spawn(fixture.scan_all());
    terminal.expect("Merge main@origin");
    terminal.expect("[y/N]");
    let inputs = fixture.read_log("scan-input");
    assert!(inputs.lines().any(|path| path == "public-0.txt"));
    assert!(!inputs.lines().any(|path| path == "main-only.pdf"));
    decline(terminal);
    assert_eq!(fixture.working_oid(), working);
    fixture.assert_no_publication();
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_historical_merge_scans_new_blob_at_the_same_main_path() {
    let fixture = historical_merge();
    let mut edited = PDF.to_vec();
    edited.extend_from_slice(b"% Different outgoing PDF bytes\n");
    fs::write(fixture.workspace.join("main-only.pdf"), &edited).unwrap();
    fixture.jj(&["describe", "-m", "Previous merge with new PDF bytes"]);
    fixture.change_main("later.txt", b"later reviewed main\n");
    let working = fixture.working_oid();
    let transcript = decline(Pty::spawn(fixture.scan_all()));
    assert!(
        transcript.contains("unsupported file format"),
        "{transcript}"
    );
    assert!(!transcript.contains("[y/N]"), "{transcript}");
    assert!(
        fixture
            .read_log("scan-input")
            .lines()
            .any(|path| path == "main-only.pdf")
    );
    assert_eq!(fixture.working_oid(), working);
    assert_eq!(
        fs::read(fixture.workspace.join("main-only.pdf")).unwrap(),
        edited
    );
    fixture.assert_no_publication();
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_conflict_is_screened_then_retained_only_after_merge_approval() {
    let fixture = Publication::with_seed(None, false, &[("collision.txt", b"base content\n")]);
    fs::write(
        fixture.workspace.join("collision.txt"),
        "outgoing content\n",
    )
    .unwrap();
    fixture.jj(&["describe", "-m", "Edit the outgoing content"]);
    fixture.jj(&["bookmark", "create", "wip/keep", "-r", "@"]);
    fixture.jj(&["bookmark", "untrack", "main@origin"]);
    let working = fixture.working_oid();
    let bookmarks = fixture.local_bookmarks();
    let main = fixture.change_main("collision.txt", b"reviewed main content\n");

    let mut terminal = Pty::spawn(fixture.scan_all());
    terminal.expect("Merge main@origin");
    terminal.expect("[y/N]");
    assert!(fixture.log.join("scan-complete").is_file());
    assert_eq!(fixture.working_oid(), working);
    assert_eq!(fixture.local_bookmarks(), bookmarks);
    assert_eq!(
        fs::read_to_string(fixture.workspace.join("collision.txt")).unwrap(),
        "outgoing content\n"
    );
    terminal.send("y\n");
    let (status, transcript) = terminal.finish();
    assert_eq!(status.code(), Some(3), "{transcript}");
    assert!(
        transcript.to_ascii_lowercase().contains("conflict"),
        "{transcript}"
    );
    assert_eq!(transcript.matches("[y/N]").count(), 1, "{transcript}");
    let merged = fixture.working_oid();
    assert_ne!(merged, working);
    assert_eq!(
        fixture.git(&["rev-parse", &format!("{merged}^2")]).trim(),
        main
    );
    assert_eq!(
        fixture
            .observe_jj(&[
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "if(conflict, \"yes\", \"no\")"
            ])
            .trim(),
        "yes"
    );
    assert_eq!(fixture.local_bookmarks(), bookmarks);
    fixture.assert_no_publication();
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_conflicted_scan_policy_is_never_read_from_a_single_git_side() {
    let ours = PDF_POLICY.replace("Synthetic fixture marker", "Outgoing fixture marker");
    let theirs = PDF_POLICY.replace("Synthetic fixture marker", "Reviewed fixture marker");
    for (file, base, outgoing, reviewed) in [
        (".gitleaks.toml", PDF_POLICY, ours.as_str(), theirs.as_str()),
        ("rules.toml", PDF_POLICY, ours.as_str(), theirs.as_str()),
        (
            ".gitleaksignore",
            "# baseline\n",
            "# outgoing\n",
            "# reviewed\n",
        ),
    ] {
        let mut seed = vec![(file, base.as_bytes())];
        if file == "rules.toml" {
            seed.push((".gitleaks.toml", b"[extend]\npath = 'rules.toml'\n"));
        }
        let fixture = Publication::with_seed(None, false, &seed);
        fs::write(fixture.workspace.join(file), outgoing).unwrap();
        fixture.jj(&["describe", "-m", "Edit outgoing policy"]);
        let working = fixture.working_oid();
        let bookmarks = fixture.local_bookmarks();
        fixture.change_main(file, reviewed.as_bytes());

        let diagnostic = decline(Pty::spawn(fixture.scan_all()));
        assert!(
            diagnostic.to_ascii_lowercase().contains("conflict"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("[y/N]"), "{diagnostic}");
        assert!(
            !diagnostic.contains("Proposed pull request"),
            "{diagnostic}"
        );
        assert!(!fixture.log.join("scan-complete").exists(), "{diagnostic}");
        assert_eq!(fixture.working_oid(), working);
        assert_eq!(fixture.local_bookmarks(), bookmarks);
        assert_eq!(
            fs::read_to_string(fixture.workspace.join(file)).unwrap(),
            outgoing
        );
        fixture.assert_no_publication();
    }
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_unavailable_detached_operations_refuse_only_diverged_all() {
    let diverged = Publication::new(None, false);
    diverged.change(0);
    let working = diverged.working_oid();
    let bookmarks = diverged.local_bookmarks();
    diverged.change_main("main.txt", b"reviewed main\n");
    let mut command = diverged.dots(&["publish", "--all"]);
    command.env("PUBLISH_NO_DETACHED", "1");
    let diagnostic = decline(Pty::spawn(command));
    let lower = diagnostic.to_ascii_lowercase();
    assert!(
        lower.contains("jj") && lower.contains("0.41"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("[y/N]"), "{diagnostic}");
    assert!(
        !diagnostic.contains("Proposed pull request"),
        "{diagnostic}"
    );
    assert!(!diverged.log.join("scan-complete").exists(), "{diagnostic}");
    assert_eq!(diverged.working_oid(), working);
    assert_eq!(diverged.local_bookmarks(), bookmarks);
    assert!(diverged.read_log("jj").contains("--no-integrate-operation"));
    diverged.assert_no_publication();

    let based = Publication::new(None, false);
    based.change(0);
    let mut command = based.dots(&["publish", "--all"]);
    command.env("PUBLISH_NO_DETACHED", "1");
    based.approve(command, "@", &["public-0.txt"]);
    assert!(!based.read_log("jj").contains("--no-integrate-operation"));
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_diverged_all_rechecks_late_working_changes_without_integrating_candidate() {
    for at_publication in [false, true] {
        let fixture = Publication::new(None, false);
        fixture.change(0);
        fixture.jj(&["bookmark", "untrack", "main@origin"]);
        let original = fixture.working_oid();
        let bookmarks = fixture.local_bookmarks();
        let main = fixture.change_main("main.txt", b"reviewed main\n");

        let mut terminal = Pty::spawn(fixture.scan_all());
        terminal.expect("Merge main@origin");
        terminal.expect("[y/N]");
        assert!(fixture.log.join("scan-complete").is_file());
        assert_eq!(fixture.working_oid(), original);
        let expected = if at_publication {
            terminal.send("y\n");
            terminal.expect("[y/N]");
            let merged = fixture.working_oid();
            assert_ne!(merged, original);
            assert_eq!(
                fixture.git(&["rev-parse", &format!("{merged}^2")]).trim(),
                main
            );
            merged
        } else {
            original
        };
        let late = fixture.workspace.join("late.txt");
        fs::write(&late, "late outgoing edit\n").unwrap();
        terminal.send("y\n");
        let (status, diagnostic) = terminal.finish();
        assert_eq!(status.code(), Some(1), "{diagnostic}");
        assert!(
            diagnostic.to_ascii_lowercase().contains("chang"),
            "{diagnostic}"
        );
        let prompts = if at_publication { 2 } else { 1 };
        assert_eq!(diagnostic.matches("[y/N]").count(), prompts, "{diagnostic}");
        assert_eq!(fixture.working_oid(), expected);
        assert_eq!(fixture.local_bookmarks(), bookmarks);
        assert_eq!(fs::read_to_string(late).unwrap(), "late outgoing edit\n");
        assert_eq!(fixture.workspace.join("main.txt").exists(), at_publication);
        fixture.assert_no_publication();
    }
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_large_native_debug_stream_is_drained_without_disclosure() {
    let fixture = Publication::new(None, false);
    fixture.change(0);
    fixture.shim(
        "gitleaks",
        r#"if [ "${1:-}" = --version ] || [ "${1:-}" = version ]; then
    printf '%s\n' 8.30.1
    exit 0
fi
[ "${1:-}" = dir ]
[ -f public-0.txt ]
/usr/bin/awk 'BEGIN { for (i=0; i<20000; i++) printf "11:00PM DBG skipping file: global allowlist path=synthetic.pdf\n" }' >&2
: > "$PUBLISH_LOG/scan-complete"
"#,
    );
    let mut terminal = Pty::spawn(fixture.dots(&["publish"]));
    terminal.expect("[y/N]");
    assert!(fixture.log.join("scan-complete").is_file());
    let transcript = decline(terminal);
    assert!(!transcript.contains("skipping file: global allowlist"));
    assert!(!transcript.contains("synthetic.pdf"));
    assert!(transcript.len() < 16 * 1024);
    fixture.assert_no_publication();
}

#[test]
#[ignore = "P-10 publish approval cancellation"]
fn new_publish_cancels_at_approval_without_mutation_and_records_a_report() {
    let fixture = Publication::new(None, false);
    fixture.change(0);
    let bookmark = "wip/interrupt";
    fixture.jj(&["bookmark", "create", bookmark, "-r", "@"]);
    let bookmarks = fixture.jj(&["bookmark", "list"]);
    let working = fixture.jj(&["log", "--no-graph", "-r", "@", "-T", "commit_id"]);
    let command = fixture.dots(&["publish", "--bookmark", bookmark]);
    let state = command
        .get_envs()
        .find_map(|(name, value)| (name == "XDG_STATE_HOME").then_some(value).flatten())
        .map(PathBuf::from)
        .expect("private fixture state");
    let reports = state.join("dots/reports");
    let report_paths = || match fs::read_dir(&reports) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read fixture reports: {error}"),
    };
    let before = report_paths();
    let mut terminal = Pty::spawn(command);
    terminal.expect("[y/N]");
    assert!(fixture.log.join("scan-complete").is_file());
    fixture.assert_no_publication();
    terminal.send("\x03");
    let (status, transcript) = terminal.finish();
    assert_eq!(status.code(), Some(130), "{transcript}");
    fixture.assert_no_publication();
    assert_eq!(fixture.jj(&["bookmark", "list"]), bookmarks);
    assert_eq!(
        fixture.jj(&["log", "--no-graph", "-r", "@", "-T", "commit_id"]),
        working
    );
    assert!(
        !fixture.optional_log("jj").lines().any(|line| {
            [
                "bookmark create",
                "bookmark set",
                "bookmark delete",
                "bookmark move",
            ]
            .iter()
            .any(|command| line.contains(command))
        }),
        "cancelled publish attempted a bookmark mutation"
    );
    let mut publish_reports = Vec::new();
    for path in report_paths()
        .into_iter()
        .filter(|path| !before.contains(path))
    {
        let report_file = fixture_file(fixture.world.root().parent().unwrap(), &path)
            .expect("regular fixture report");
        assert_eq!(
            report_file.parent.metadata().unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(
            report_file.file.metadata().unwrap().permissions().mode() & 0o077,
            0
        );
        let report: serde_json::Value = serde_json::from_reader(report_file.file).unwrap();
        if report["command"] == "publish" {
            publish_reports.push(report);
        }
    }
    assert_eq!(publish_reports.len(), 1);
    let report = &publish_reports[0];
    assert_eq!(report["status"], 130);
    assert_eq!(report["cancelled"], true);
    assert!(
        report["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|stage| stage["label"] == "Publish source" && stage["outcome"] == "cancelled")
    );
}

#[test]
#[ignore = "P-10"]
fn unchanged_default_publication_scans_exactly_the_unpublished_commits_before_asking() {
    // A secret already in remote main is outside this publication's outgoing set.
    // The removed-secret and merge-resolution cases independently prove coverage
    // within the outgoing set using the same real scanner.
    let fixture = Publication::new(Some("Sam"), true);
    fixture.change(0);
    let mut command = fixture.dots(&["publish"]);
    command.env("DOTS_PUBLISH_MODEL", MODEL);
    fixture.real_scan(&mut command);
    fixture.approve(command, "@", &["public-0.txt"]);
    assert!(fixture.read_log("body").contains(MODEL));
    assert!(fixture.read_log("body").contains("Sam"));
}

#[test]
#[ignore = "P-10"]
fn unchanged_real_gitleaks_refuses_a_token_committed_and_then_removed() {
    let fixture = token_history();
    fixture.reject_secret("wip/token", false);
}

fn token_history() -> Publication {
    let fixture = Publication::new(None, false);
    write_token(&fixture.workspace);
    fixture.jj(&["describe", "-m", "Add synthetic token"]);
    fixture.jj(&["new"]);
    fs::remove_file(fixture.workspace.join("synthetic-token.txt")).unwrap();
    fixture.jj(&["describe", "-m", "Remove synthetic token"]);
    fixture.jj(&["bookmark", "create", "wip/token", "-r", "@"]);
    fixture
}

#[test]
#[ignore = "P-10"]
fn new_publish_never_previews_the_rejected_secret() {
    let token = synthetic_token();
    for terminal in [false, true] {
        for (fixture, bookmark) in [
            (token_history(), "wip/token"),
            (merge_resolution(), "wip/merge"),
        ] {
            let transcript = fixture.reject_secret(bookmark, terminal);
            assert!(!transcript.contains(&token));
            assert_no_preview(&transcript);
        }
    }
}

#[test]
#[ignore = "P-10"]
fn unchanged_real_gitleaks_scans_content_that_only_a_merge_resolution_adds() {
    merge_resolution().reject_secret("wip/merge", false);
}

fn merge_resolution() -> Publication {
    let fixture = Publication::new(None, false);
    let mut sides = Vec::new();
    for side in ["left", "right"] {
        fixture.jj(&["new", "main@origin", "-m", &format!("Add {side}")]);
        fs::write(fixture.workspace.join(format!("{side}.txt")), side).unwrap();
        sides.push(fixture.jj(&["log", "--no-graph", "-r", "@", "-T", "change_id"]));
    }
    fixture.jj(&[
        "new",
        sides[0].trim(),
        sides[1].trim(),
        "-m",
        "Merge both sides",
    ]);
    write_token(&fixture.workspace);
    fixture.jj(&["bookmark", "create", "wip/merge", "-r", "@"]);
    fixture
}

#[test]
#[ignore = "P-10"]
fn unchanged_default_publication_refuses_a_private_stack_above_main_origin() {
    let fixture = Publication::new(None, false);
    fixture.change(0);
    fixture.jj(&["new"]);
    fixture.change(1);
    let (_, stderr) = failure(fixture.dots(&["publish"]).output().unwrap());
    assert!(
        stderr.contains("one unpublished working change"),
        "{stderr}"
    );
    fixture.assert_no_publication();
}

#[test]
#[ignore = "P-10"]
fn unchanged_publication_uses_the_callers_jj_workspace() {
    let fixture = Publication::new(None, false);
    fixture.change(0);
    let subdirectory = fixture.workspace.join("nested");
    fs::create_dir(&subdirectory).unwrap();
    let mut command = fixture.dots(&["publish"]);
    command.current_dir(subdirectory);
    let approved = fixture.approve(command, "@", &["public-0.txt"]);
    let bytes = success(
        fixture
            .world
            .git()
            .arg("--git-dir")
            .arg(&fixture.bare)
            .args(["show", &format!("{approved}:public-0.txt")]),
    );
    assert_eq!(bytes, "fixture\n");
    assert!(!fixture.world.root().join("public-0.txt").exists());
}

#[test]
#[ignore = "P-10"]
fn unchanged_publication_outside_a_jj_workspace_of_the_configuration_stops_with_one_line() {
    let fixture = Publication::new(None, false);
    let foreign = fixture.world.root().parent().unwrap().join("foreign");
    success(
        fixture
            .world
            .git()
            .args(["clone", "--branch", "main"])
            .env("GIT_ALLOW_PROTOCOL", "file")
            .arg(&fixture.bare)
            .arg(&foreign),
    );
    success(
        fixture
            .world
            .git()
            .args([
                "remote",
                "set-url",
                "origin",
                "https://github.com/example/foreign-fixture.git",
            ])
            .current_dir(&foreign),
    );
    success(
        fixture
            .world
            .tool(&fixture.real_jj)
            .current_dir(&foreign)
            .env("GIT_ALLOW_PROTOCOL", "file")
            .args(["git", "init", "--colocate"]),
    );

    for candidate in [
        fixture.world.root(),
        &foreign,
        fixture.world.root().parent().unwrap(),
    ] {
        let (stdout, stderr) = failure(
            fixture
                .dots(&["publish"])
                .current_dir(candidate)
                .output()
                .unwrap(),
        );
        let diagnostics = format!("{stdout}{stderr}");
        assert_eq!(
            diagnostics.lines().count(),
            1,
            "{candidate:?}: {diagnostics}"
        );
        assert!(
            diagnostics.to_lowercase().contains("workspace"),
            "{diagnostics}"
        );
        assert!(
            !fixture.optional_log("jj").contains("git fetch"),
            "attempted to fetch from rejected workspace {candidate:?}"
        );
        assert!(fixture.optional_log("git-transport").is_empty());
    }
    fixture.assert_no_publication();
}

#[test]
#[ignore = "P-10"]
fn unchanged_disclosure_note_is_optional_and_names_the_configured_person() {
    for (person, model) in [
        (None, None),
        (Some("Sam"), Some(MODEL)),
        (Some("Sam"), None),
        (Some("Sam"), Some(" ")),
    ] {
        let fixture = Publication::new(person, false);
        fixture.change(0);
        let mut command = fixture.dots(&["publish"]);
        if let Some(model) = model {
            command.env("DOTS_PUBLISH_MODEL", model);
        }
        fixture.approve(command, "@", &["public-0.txt"]);
        let body = fixture.read_log("body");
        match person {
            Some(person) => {
                assert!(body.contains(person), "{body}");
                assert!(body.contains("on behalf of"), "{body}");
                if let Some(model) = model.filter(|model| !model.trim().is_empty()) {
                    assert!(body.contains(model), "{body}");
                }
            }
            None => assert!(!body.contains("on behalf of"), "{body}"),
        }
    }
}

fn set_shared_dev(fixture: &Publication, args: &[&str], already_in_main: bool) {
    fixture.change(0);
    let dev = if already_in_main {
        fixture.git(&["rev-parse", "refs/remotes/origin/main"])
    } else {
        fixture.jj(&["log", "--no-graph", "-r", "@", "-T", "commit_id"])
    };
    fixture.git(&["update-ref", "refs/heads/dev", dev.trim()]);
    if !args.is_empty() {
        fixture.jj(&["new", "@"]);
        fixture.change(1);
    }
    if args.first() == Some(&"--bookmark") {
        fixture.jj(&["bookmark", "create", args[1], "-r", "@"]);
        // This unrelated working change must not change the selected ancestry.
        fixture.jj(&["new", "main@origin"]);
        fixture.change(2);
    }
}

#[test]
#[ignore = "P-10"]
fn unchanged_shared_dev_outside_main_blocks_each_publication_mode() {
    for args in [&[][..], &["--all"], &["--bookmark", "wip/fixture-publish"]] {
        let fixture = Publication::new(None, false);
        set_shared_dev(&fixture, args, false);
        let mut command = fixture.dots(&["publish"]);
        let (out, err) = failure(command.args(args).output().unwrap());
        let diagnostic = format!("{out}{err}").to_lowercase();
        assert!(
            diagnostic.contains("focused") && diagnostic.contains("main"),
            "{diagnostic}"
        );
        assert!(!fixture.log.join("scan-complete").exists());
        fixture.assert_no_publication();
    }
}

#[test]
#[ignore = "P-10"]
fn unchanged_shared_dev_already_in_main_allows_each_publication_mode() {
    for args in [&[][..], &["--all"], &["--bookmark", "wip/fixture-publish"]] {
        let fixture = Publication::new(None, false);
        set_shared_dev(&fixture, args, true);
        let mut command = fixture.dots(&["publish"]);
        command.args(args);
        let (revision, paths) = match args {
            [] => ("@", &["public-0.txt"][..]),
            ["--all"] => ("@", &["public-0.txt", "public-1.txt"][..]),
            [_, bookmark] => (*bookmark, &["public-0.txt", "public-1.txt"][..]),
            _ => unreachable!(),
        };
        fixture.approve(command, revision, paths);
    }
}

#[test]
#[ignore = "P-10"]
fn new_non_github_repository_exits_before_checking_publication_tools() {
    let fixture = Publication::new(None, false);
    let origin = format!("file://{}", fixture.bare.display());
    fixture.git(&["remote", "set-url", "origin", &origin]);
    success(
        fixture
            .world
            .git()
            .args(["remote", "set-url", "origin", &origin]),
    );
    fs::write(
        fixture.world.root().join("config.toml"),
        format!("[vars]\ndots_profiles = \"laptop\"\ndots_source_repo = {origin:?}\n"),
    )
    .unwrap();
    fixture.deny_tool_probes();
    let diagnostics = fixture.denied_without_contact(1, true);
    assert!(diagnostics.contains("GitHub"), "{diagnostics}");
}

#[test]
#[ignore = "P-10"]
fn new_invalid_repository_setting_exits_two_before_tool_probes() {
    for config in [
        "[vars]\ndots_source_repo = \"invalid fixture setting\"\n",
        "[vars\ndots_source_repo = \"example/p07-fixture\"\n",
    ] {
        let fixture = Publication::new(None, false);
        fs::write(fixture.world.root().join("config.toml"), config).unwrap();
        fixture.deny_tool_probes();
        fixture.denied_without_contact(2, true);
    }
}

#[test]
#[ignore = "P-10"]
fn new_publish_rejects_absent_unsafe_or_mismatched_raw_origins_before_tool_probes() {
    let credential = "P07_FIXTURE_PASSWORD_CANARY";
    let credential_url = format!("https://fixture-user:{credential}@github.com/{REPOSITORY}.git");
    let alias = "https://github.com/example/other-fixture.git";
    for use_live_checkout in [false, true] {
        for raw in [
            None,
            Some("invalid-fixture-url"),
            Some(credential_url.as_str()),
            Some(alias),
        ] {
            let fixture = Publication::new(None, false);
            let checkout = if use_live_checkout {
                fixture.world.root()
            } else {
                &fixture.workspace
            };
            if let Some(raw) = raw {
                fixture.git_in(checkout, &["remote", "set-url", "origin", raw]);
                if raw == alias {
                    fixture.git_in(
                        checkout,
                        &["config", &format!("url.{GITHUB_ORIGIN}.insteadOf"), alias],
                    );
                    assert_eq!(
                        fixture
                            .git_in(checkout, &["remote", "get-url", "origin"])
                            .trim(),
                        GITHUB_ORIGIN
                    );
                }
            } else {
                fixture.git_in(checkout, &["remote", "remove", "origin"]);
            }
            fixture.deny_tool_probes();
            let diagnostic = fixture.denied_without_contact(1, true);
            let summary = diagnostic.to_ascii_lowercase();
            assert!(
                summary.contains("github")
                    || summary.contains("origin")
                    || summary.contains("repository"),
                "{diagnostic}"
            );
            assert!(!diagnostic.contains(credential), "{diagnostic}");
            if let Some(raw) = raw {
                assert!(
                    !diagnostic.contains(raw),
                    "echoed a rejected raw origin: {diagnostic}"
                );
            }
        }
    }
}

#[test]
#[ignore = "P-10"]
fn new_publish_checks_effective_fetch_and_every_push_url_before_contact() {
    for variant in ["fetch-rewrite", "push-rewrite", "multiple-push-urls"] {
        let fixture = Publication::new(None, false);
        let rejected = fixture.world.bare_remote("rejected.git");
        let redirected = format!("file://{}", rejected.display());
        let matching_push = format!("git@github.com:{REPOSITORY}.git");
        match variant {
            "fetch-rewrite" => {
                fixture.git(&[
                    "config",
                    &format!("url.{redirected}.insteadOf"),
                    GITHUB_ORIGIN,
                ]);
                fixture.git(&["config", "--add", "remote.origin.pushurl", &matching_push]);
            }
            "push-rewrite" => {
                fixture.git(&[
                    "config",
                    &format!("url.{redirected}.pushInsteadOf"),
                    GITHUB_ORIGIN,
                ]);
            }
            _ => {
                fixture.git(&["config", "--add", "remote.origin.pushurl", &matching_push]);
                fixture.git(&["config", "--add", "remote.origin.pushurl", &redirected]);
            }
        }
        assert_eq!(
            fixture
                .git(&["config", "--get", "remote.origin.url"])
                .trim(),
            GITHUB_ORIGIN
        );
        let fetch = fixture.git(&["remote", "get-url", "origin"]);
        let pushes = fixture.git(&["remote", "get-url", "--push", "--all", "origin"]);
        assert_eq!(fetch.trim() == redirected, variant == "fetch-rewrite");
        assert_eq!(
            pushes.lines().any(|url| url == redirected),
            variant != "fetch-rewrite"
        );
        if variant == "multiple-push-urls" {
            assert_eq!(pushes.lines().count(), 2);
            assert!(pushes.lines().any(|url| url == matching_push));
        }
        let diagnostic = fixture.denied_without_contact(1, false);
        assert!(!diagnostic.contains(&redirected), "{diagnostic}");
    }
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_dirty_bookmarked_stack_keeps_exact_bookmark_on_both_declines() {
    let fixture = Publication::new(None, false);
    fixture.change(0);
    let bookmark = "wip/dotfiles-all-dirty";
    fixture.jj(&["bookmark", "create", bookmark, "-r", "@"]);
    fixture.jj(&["bookmark", "untrack", "main@origin"]);
    let original = fixture.working_oid();
    let local = fixture.local_bookmarks();
    let branch = format!("refs/heads/{bookmark}");
    fixture.git(&[
        "push",
        fixture.bare.to_str().unwrap(),
        &format!("{original}:{branch}"),
    ]);
    let remote = || {
        fixture.git(&[
            "--git-dir",
            fixture.bare.to_str().unwrap(),
            "rev-parse",
            &branch,
        ])
    };
    fs::write(fixture.log.join("existing-pr.json"), format!(
        r#"[{{"number":17,"state":"OPEN","url":"{PR_URL}","isCrossRepository":false,"baseRefName":"release"}}]"#,
    )).unwrap();
    let reviewed = fixture.change_main("late-main.txt", b"reviewed main\n");
    fs::write(fixture.workspace.join("public-0.txt"), "dirty fixture\n").unwrap();

    let mut first = Pty::spawn(fixture.scan_all());
    first.expect("Merge main@origin");
    first.expect("[y/N]");
    decline(first);
    assert_eq!(fixture.working_oid(), original);
    assert_eq!(fixture.local_bookmarks(), local);
    assert_eq!(remote().trim(), original);

    let mut second = Pty::spawn(fixture.scan_all());
    second.expect("Merge main@origin");
    second.expect("[y/N]");
    second.send("y\n");
    second.expect("[y/N]");
    let merged = fixture.working_oid();
    assert_ne!(merged, original);
    assert_eq!(
        fixture.git(&["rev-parse", &format!("{merged}^2")]).trim(),
        reviewed
    );
    assert_eq!(
        fixture.git(&["show", &format!("{merged}:public-0.txt")]),
        "dirty fixture\n"
    );
    decline(second);
    assert_eq!(fixture.working_oid(), merged);
    assert_eq!(fixture.local_bookmarks(), local);
    assert_eq!(remote().trim(), original);
    assert!(!fixture.optional_log("gh").contains("pr edit"));
    assert!(!fixture.optional_log("gh").contains("pr create"));
    assert!(
        !fixture
            .optional_log("git-transport")
            .lines()
            .any(|entry| entry == "push")
    );

    fs::remove_file(fixture.log.join("scan-status")).unwrap();
    let mut third = Pty::spawn(fixture.scan_all());
    third.expect("[y/N]");
    assert_eq!(fixture.read_log("scan-status"), "0\n");
    third.send("y\n");
    let (status, transcript) = third.finish();
    assert!(
        status.success() && transcript.contains(PR_URL),
        "{status}\n{transcript}"
    );
    assert!(!transcript.contains("Merge main@origin"), "{transcript}");
    assert_eq!(remote().trim(), merged);
    let gh = fixture.read_log("gh");
    assert!(
        gh.lines()
            .any(|line| line.split_ascii_whitespace().take(5).eq([
                "pr",
                "edit",
                "--repo",
                "github.com/example/p07-fixture",
                "17"
            ]) && !line
                .split_ascii_whitespace()
                .any(|arg| arg == "--base" || arg.starts_with("--base="))),
        "{gh}"
    );
    assert!(!gh.contains("pr create"), "{gh}");
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_divergent_operation_heads_never_reconcile_during_verification() {
    let fixture = Publication::new(None, false);
    fixture.change(0);
    let older = fixture.observe_jj(&["operation", "log", "--no-graph", "--limit=1", "-T", "id"]);
    fixture.jj(&["describe", "-m", "First independent operation"]);
    fixture.jj(&[
        "--at-op",
        older.trim(),
        "--ignore-working-copy",
        "describe",
        "-m",
        "Second independent operation",
    ]);
    let heads = || {
        let mut heads: Vec<_> = fs::read_dir(fixture.workspace.join(".jj/repo/op_heads/heads"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        heads.sort();
        heads
    };
    let refs = || fixture.git(&["for-each-ref", "--format=%(refname) %(objectname)"]);
    let before = heads();
    assert_eq!(before.len(), 2);
    let original_refs = refs();
    for missing_capability in [false, true] {
        let mut command = fixture.scan_all();
        if missing_capability {
            command.env("PUBLISH_NO_DETACHED", "1");
        }
        let transcript = refusal(command, false);
        assert!(
            transcript.contains("JJ has multiple operation heads"),
            "{transcript}"
        );
        assert_no_preview(&transcript);
        assert_eq!(heads(), before);
        assert_eq!(refs(), original_refs);
        assert!(fixture.optional_log("git-transport").is_empty());
        assert!(!fixture.log.join("scan-complete").exists());
    }
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_interrupted_integration_retry_gives_native_recovery_command() {
    let fixture = Publication::new(None, false);
    fixture.change(0);
    fixture.jj(&["bookmark", "untrack", "main@origin"]);
    let original = fixture.working_oid();
    let checked_out = fixture.workspace.join("reviewed-after-recovery");
    let main = fixture.change_main("reviewed-after-recovery", b"reviewed\n");
    let mut command = fixture.scan_all();
    command.env("PUBLISH_INTERRUPT_ON_INTEGRATE", "1");
    let mut terminal = Pty::spawn(command);
    terminal.expect("Merge main@origin");
    terminal.expect("[y/N]");
    terminal.send("y\n");
    let (status, transcript) = terminal.finish();
    assert_eq!(status.code(), Some(130), "{transcript}");
    assert!(fixture.log.join("integrated").is_file());
    let merged = fixture.working_oid();
    assert_ne!(merged, original);
    assert_eq!(
        fixture.git(&["rev-parse", &format!("{merged}^2")]).trim(),
        main
    );
    assert!(!checked_out.exists());
    let transcript = refusal(fixture.scan_all(), false);
    assert!(
        transcript.contains("jj workspace update-stale"),
        "{transcript}"
    );
    assert_no_preview(&transcript);
    assert_eq!(fixture.working_oid(), merged);
    assert!(!checked_out.exists());
    fixture.assert_no_publication();

    fixture.jj(&["workspace", "update-stale"]);
    assert_eq!(fixture.working_oid(), merged);
    assert_eq!(fs::read_to_string(checked_out).unwrap(), "reviewed\n");
    let mut again = Pty::spawn(fixture.scan_all());
    again.expect("[y/N]");
    let transcript = decline(again);
    assert!(!transcript.contains("Merge main@origin"), "{transcript}");
    assert_eq!(fixture.working_oid(), merged);
    fixture.assert_no_publication();
}

fn nested_archive(note: &[u8]) -> Vec<u8> {
    zipped("inner.zip", &zipped("note.txt", note))
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_conflict_projection_does_not_rescan_main_only_archive() {
    let fixture = Publication::with_seed(None, false, &[("collision.txt", b"base content\n")]);
    fs::write(
        fixture.workspace.join("collision.txt"),
        "outgoing content\n",
    )
    .unwrap();
    fixture.jj(&["describe", "-m", "Edit a conflicted path"]);
    fixture.jj(&["bookmark", "untrack", "main@origin"]);
    let original = fixture.working_oid();
    let bookmarks = fixture.local_bookmarks();
    let archive = nested_archive(b"reviewed main archive\n");
    fixture.change_main("main-only.zip", &archive);
    let main = fixture.change_main("collision.txt", b"reviewed main content\n");
    let mut first = Pty::spawn(fixture.scan_all());
    first.expect("Merge main@origin");
    first.expect("[y/N]");
    assert_eq!(fixture.read_log("scan-status"), "0\n");
    decline(first);
    assert_eq!(fixture.working_oid(), original);
    assert_eq!(fixture.local_bookmarks(), bookmarks);

    let mut accept = Pty::spawn(fixture.scan_all());
    accept.expect("Merge main@origin");
    accept.expect("[y/N]");
    accept.send("y\n");
    let (status, transcript) = accept.finish();
    assert_eq!(status.code(), Some(3), "{transcript}");
    assert_eq!(transcript.matches("[y/N]").count(), 1, "{transcript}");
    let merged = fixture.working_oid();
    assert_eq!(
        fixture.git(&["rev-parse", &format!("{merged}^2")]).trim(),
        main
    );
    let raw = fixture.git(&["ls-tree", "-r", "--name-only", &merged]);
    assert!(
        raw.lines()
            .any(|line| line.starts_with(".jjconflict-side-") && line.ends_with("/main-only.zip")),
        "{raw}"
    );
    assert!(
        !fixture
            .optional_log("scan-stderr")
            .contains("exceeds max archive depth")
    );
    assert_eq!(fixture.local_bookmarks(), bookmarks);
    fixture.assert_no_publication();
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_conflict_projection_still_screens_new_logical_resolution() {
    const RULES: &[u8] = br#"[[rules]]
id = "fixture-logical-resolution"
description = "Adjacent synthetic fixture markers"
regex = '''(?s)\AFIXTURE_LEFT_MARKER\n.*\nFIXTURE_RIGHT_MARKER\n\z'''
"#;
    for victim in ["resolution.txt", ".jjconflict-side-0/resolution.txt"] {
        let base = b"base first\nsecond\nunchanged middle\nfourth\nbase last\n";
        let fixture = Publication::with_seed(
            None,
            false,
            &[
                (".gitleaks.toml", RULES),
                ("collision.txt", b"base\n"),
                (victim, base),
            ],
        );
        fs::write(
            fixture.workspace.join(victim),
            "FIXTURE_LEFT_MARKER\nsecond\nunchanged middle\nfourth\nbase last\n",
        )
        .unwrap();
        fs::write(fixture.workspace.join("collision.txt"), "outgoing\n").unwrap();
        fixture.jj(&["describe", "-m", "Edit one side of the logical resolution"]);
        let original = fixture.working_oid();
        fixture.change_main(
            victim,
            b"base first\nsecond\nunchanged middle\nfourth\nFIXTURE_RIGHT_MARKER\n",
        );
        fixture.change_main("collision.txt", b"reviewed\n");
        let transcript = refusal(fixture.scan_all(), true);
        assert!(
            transcript.contains("gitleaks found potential secrets"),
            "{transcript}"
        );
        assert_no_preview(&transcript);
        assert!(!transcript.contains("FIXTURE_LEFT_MARKER"));
        assert!(!transcript.contains("FIXTURE_RIGHT_MARKER"));
        assert_eq!(fixture.read_log("scan-status"), "20\n");
        let report: Vec<serde_json::Value> =
            serde_json::from_str(&fixture.read_log("findings.json")).unwrap();
        assert!(
            report.iter().any(|finding| {
                finding["RuleID"] == "fixture-logical-resolution"
                    && finding["Secret"] == "REDACTED"
                    && finding["File"]
                        .as_str()
                        .is_some_and(|file| file.ends_with(victim))
            }),
            "{report:?}"
        );
        assert_eq!(fixture.working_oid(), original);
        fixture.assert_no_publication();
    }
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_legitimate_jjconflict_name_is_scanned() {
    let fixture = Publication::new(None, false);
    let directory = fixture.workspace.join(".jjconflict-side-0");
    fs::create_dir(&directory).unwrap();
    fs::write(
        directory.join("outgoing.zip"),
        nested_archive(b"outgoing archive\n"),
    )
    .unwrap();
    fixture.jj(&[
        "describe",
        "-m",
        "Add a normal directory with a similar name",
    ]);
    let original = fixture.working_oid();
    let mut command = fixture.dots(&["publish"]);
    fixture.real_scan(&mut command);
    let transcript = refusal(command, true);
    assert!(transcript.contains("one-level scan limit"), "{transcript}");
    assert_no_preview(&transcript);
    assert_eq!(fixture.read_log("scan-status"), "0\n");
    assert!(fixture.read_log("scan-stderr").lines().any(|line| {
        line.contains("exceeds max archive depth")
            && line.contains(".jjconflict-side-0/outgoing.zip")
    }));
    assert_eq!(fixture.working_oid(), original);
    fixture.assert_no_publication();
}

fn merged_archive_fixture(change_and_restore: bool) -> Publication {
    const ARCHIVE: &str = "inherited.zip";
    let fixture = Publication::new(None, false);
    fixture.change(0);
    let first = fixture.working_oid();
    let archive = nested_archive(b"reviewed main archive\n");
    let main = fixture.change_main(ARCHIVE, &archive);
    fixture.git(&[
        "fetch",
        "--no-tags",
        fixture.bare.to_str().unwrap(),
        "refs/heads/main:refs/remotes/origin/main",
    ]);
    fixture.jj(&["git", "import"]);
    fixture.jj(&["new", &main, "-m", "Second outgoing parent"]);
    if change_and_restore {
        fs::write(
            fixture.workspace.join(ARCHIVE),
            nested_archive(b"outgoing changed archive\n"),
        )
        .unwrap();
        fixture.jj(&[
            "describe",
            "-m",
            "Change inherited archive in unpublished history",
        ]);
        fixture.jj(&["new", "-m", "Restore reviewed archive bytes"]);
        fs::write(fixture.workspace.join(ARCHIVE), archive).unwrap();
    }
    fixture.change(1);
    let second = fixture.working_oid();
    fixture.jj(&["new", &first, &second, "-m", "Merge both outgoing parents"]);
    fixture.jj(&["bookmark", "create", "wip/merged-archive", "-r", "@"]);
    fixture
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_archive_inherited_through_outgoing_parent_is_not_rescanned() {
    let fixture = merged_archive_fixture(false);
    let command = fixture.dots(&["publish", "--bookmark", "wip/merged-archive"]);
    fixture.approve(
        command,
        "wip/merged-archive",
        &["public-0.txt", "public-1.txt"],
    );
    assert!(
        !fixture
            .read_log("scan-stderr")
            .contains("exceeds max archive depth")
    );
}

#[test]
#[ignore = "P-10 detached aggregate screening"]
fn new_changed_then_restored_archive_in_outgoing_parent_is_scanned() {
    let fixture = merged_archive_fixture(true);
    let head = fixture.working_oid();
    let mut command = fixture.dots(&["publish", "--bookmark", "wip/merged-archive"]);
    fixture.real_scan(&mut command);
    let transcript = refusal(command, true);
    assert!(transcript.contains("one-level scan limit"), "{transcript}");
    assert_no_preview(&transcript);
    assert_eq!(fixture.read_log("scan-status"), "0\n");
    assert!(
        fixture.read_log("scan-stderr").lines().any(
            |line| line.contains("inherited.zip") && line.contains("exceeds max archive depth")
        )
    );
    assert_eq!(fixture.working_oid(), head);
    fixture.assert_no_publication();
}
