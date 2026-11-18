#[path = "p07/isolation.rs"]
mod isolation;
mod support;

#[path = "p07/pty.rs"]
mod pty;

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use isolation::{IsolatedWorld, executable_on_path};
use pty::Pty;

const REPOSITORY: &str = "example/p07-fixture";
const GITHUB_ORIGIN: &str = "https://github.com/example/p07-fixture.git";
const PR_URL: &str = "https://github.com/example/p07-fixture/pull/17";
const MODEL: &str = "Fixture Model";

struct Publication {
    world: IsolatedWorld,
    bare: PathBuf,
    workspace: PathBuf,
    tools: PathBuf,
    log: PathBuf,
    real_git: PathBuf,
    real_jj: PathBuf,
    real_gitleaks: Option<PathBuf>,
}

impl Publication {
    fn new(on_behalf_of: Option<&str>, baseline_secret: bool) -> Self {
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
                symlink(tool, tools.join(name)).unwrap();
            }
        }

        let real_git =
            executable_on_path(&world.dots(), "git").expect("publish scenarios require Git");
        symlink(&real_git, log.join("scan-bin/git")).unwrap();
        let real_jj =
            executable_on_path(&world.dots(), "jj").expect("publish scenarios require jj");
        let real_gitleaks = executable_on_path(&world.dots(), "gitleaks");
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
classify() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --no-pager|--color=never|--ignore-working-copy) shift ;;
            --config) [ "$2" = git.abandon-unreachable-commits=false ]; shift 2 ;;
            --repository|-R) [ "$2" = "$PUBLISH_WORKSPACE" ]; shift 2 ;;
            *) break ;;
        esac
    done
    case "${1-} ${2-}" in
        'git fetch'|'git push') printf transport; return ;;
        'git root') printf local; return ;;
    esac
    case "${1-}" in
        bookmark|describe|diff|log|new|root|show|status|--version|version) printf local ;;
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
    local) exec "$PUBLISH_JJ" "$@" ;;
    *) exit 88 ;;
esac
"#,
        );
        self.shim(
            "gh",
            r#"printf '%s\n' "$*" >> "$PUBLISH_LOG/gh"
case "${1-} ${2-}" in
    'pr list') printf '%s\n' '[]' ;;
    'pr create'|'pr edit')
        shift 2
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --body-file|-F)
                    if [ "$2" = - ]; then
                        cat > "$PUBLISH_LOG/body"
                    else
                        cat "$2" > "$PUBLISH_LOG/body"
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
case "$1" in
    git)
        range=
        for arg do
            case "$arg" in --log-opts=*) range=${arg##* } ;; esac
        done
        case "$range" in *[!a-f0-9.]*|'') exit 88 ;; esac
        "$PUBLISH_GIT" -C "$PUBLISH_WORKSPACE" log --format= --name-only "$range" -- >> "$PUBLISH_LOG/scan-input"
        ;;
    dir)
        case "$(pwd -P)" in "$PUBLISH_LOG"/tmp/*) ;; *) exit 88 ;; esac
        for path in public-*.txt synthetic-token.txt; do
            if [ -f "$path" ] && [ ! -L "$path" ]; then
                printf '%s\n' "$path" >> "$PUBLISH_LOG/scan-input"
            fi
        done
        ;;
    *) exit 88 ;;
esac
status=0
if [ "${PUBLISH_REAL_SCAN-}" = 1 ]; then
    command=$1
    shift
    PATH="$PUBLISH_LOG/scan-bin" "$PUBLISH_GITLEAKS" "$command" \
        --report-format json --report-path "$PUBLISH_LOG/findings.json" "$@" || status=$?
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

    fn dots(&self, args: &[&str]) -> Command {
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
            .env_remove("DOTS_PUBLISH_MODEL");
        if let Some(gitleaks) = &self.real_gitleaks {
            command.env("PUBLISH_GITLEAKS", gitleaks);
        }
        command
    }

    fn real_scan(&self, command: &mut Command) {
        assert!(
            self.real_gitleaks.is_some(),
            "real-scanner scenario requires gitleaks"
        );
        command.env("PUBLISH_REAL_SCAN", "1");
    }

    fn read_log(&self, name: &str) -> String {
        fs::read_to_string(self.log.join(name)).unwrap()
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

    fn approve(&self, command: Command, revision: &str, expected_paths: &[&str]) -> String {
        let approved = self.jj(&["log", "--no-graph", "-r", revision, "-T", "commit_id"]);
        let approved = approved.trim();
        let mut terminal = Pty::spawn(command);
        terminal.expect("[y/N]");
        assert!(
            self.log.join("scan-complete").is_file(),
            "confirmation preceded scanner completion"
        );
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
        let head = calls
            .lines()
            .find(|line| line.starts_with("pr create "))
            .and_then(|line| {
                line.split_ascii_whitespace()
                    .skip_while(|word| *word != "--head")
                    .nth(1)
            })
            .expect("PR creation must name its published head");
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
        approved.to_owned()
    }

    fn reject_secret(&self, bookmark: &str, terminal: bool) -> String {
        let mut command = self.dots(&["publish", "--bookmark", bookmark]);
        self.real_scan(&mut command);
        let transcript = if terminal {
            let (status, transcript) = Pty::spawn(command).finish();
            assert_eq!(status.code(), Some(1));
            transcript
        } else {
            let (stdout, stderr) = failure(command.output().unwrap());
            format!("{stdout}{stderr}")
        };
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
            transcript.contains("gitleaks"),
            "publish did not report a scanner failure"
        );
        self.assert_no_publication();
        transcript
    }
}

fn success(command: &mut Command) -> String {
    let result = command.output().unwrap();
    assert!(result.status.success(), "{command:?}\n{result:?}");
    String::from_utf8(result.stdout).unwrap()
}

fn failure(output: Output) -> (String, String) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
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
    for (fixture, bookmark) in [
        (token_history(), "wip/token"),
        (merge_resolution(), "wip/merge"),
    ] {
        let transcript = fixture.reject_secret(bookmark, true);
        assert!(!transcript.contains(&token));
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
