//! P-07 executable receipt scenarios. See the P-07 scenario inventory.
#[path = "p07/isolation.rs"]
mod isolation;
#[path = "p07/pty.rs"]
mod pty;
mod support;

use isolation::{IsolatedWorld, executable_on_path};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Instant;
use support::FixtureCommand;

const ORIGIN: &str = "https://github.com/dots-tests/receipt.git";
const MISE: &str = r#"#!/bin/sh
set -eu
if [ "$1" = -C ]; then shift 2; fi
case "$*" in
  'exec -- '*) shift 2; exec "$@" ;;
  --version) printf '%s\n' 2026.11.8 ;;
  *'vars.dots_source_repo') printf '%s\n' dots-tests/receipt ;;
  *'vars.dots_profiles') printf '%s\n' test ;;
  *'dotfiles paths --json'*|*'dotfiles --local paths --json'*) printf '%s\n' '{"entries":[],"invalid":[]}' ;;
  *'dotfiles status --json'*) printf '%s\n' '{"files":[],"edits":[]}' ;;
  *'.encrypt'*) exit 1 ;;
  *'dotfiles save'*|*'dotfiles apply'*) exit 0 ;;
  *'config ls --json'*) jq -n --arg path "$MISE_CONFIG_DIR/config.toml" '[{path: $path}]' ;;
  *'which dots-up'*) exit 1 ;;
  *) printf 'unexpected mise query: %s\n' "$*" >&2; exit 1 ;;
esac
"#;

const SCANNER: &str = r#"#!/bin/sh
set -eu
controls=$(CDPATH='' cd -- "${0%/*}/.." && pwd -P)
world=${controls%/*}
mode=''
target=''
report='-'
while [ "$#" -gt 0 ]; do
    case "$1" in
      --report-path|-r) report=$2; shift 2 ;;
      --report-path=*) report=${1#*=}; shift ;;
      --config|-c|--gitleaks-ignore-path|--report-format|-f|--log-level|-l|--log-opts)
        shift 2 ;;
      --)
        shift
        if [ "$#" -gt 0 ]; then target=$1; fi
        break ;;
      -*) shift ;;
      git|stdin|dir)
        if [ -z "$mode" ]; then mode=$1; else target=$1; fi
        shift ;;
      *) target=$1; shift ;;
    esac
done

emit() {
    case "$report" in
      ''|-|/dev/stdout) printf '%s\n' "$1" ;;
      /*)
        parent=$(CDPATH='' cd -- "${report%/*}" && pwd -P)
        case "$parent" in
          "$world"/*) [ ! -L "$report" ]; printf '%s\n' "$1" > "$report" ;;
          *) exit 2 ;;
        esac ;;
      *) exit 2 ;;
    esac
}

if [ "$mode" = git ]; then emit '[]'; exit 0; fi
if [ -f "$controls/scanner-failure" ]; then
    case "$(cat "$controls/scanner-failure")" in
      error) exit 2 ;;
      empty) exit 0 ;;
      malformed) emit 'not JSON'; exit 0 ;;
    esac
fi

finding=0
scan_text() {
    if [ -f "$controls/scanned-content" ]; then
        printf '%s %s\n' "$mode" "$1" >> "$controls/scanned-content"
    fi
    case "$1" in
      *STALL_SCAN*)
        printf 'stalled\n' > "$controls/scanner-stalled"
        trap '' TERM
        exec sleep 60 ;;
      *SCAN_ALERT*) finding=1 ;;
      *DISPLAY_ONLY*) if [ "$mode" = stdin ]; then finding=1; fi ;;
      *PATH_RULE*) if [ "${2##*/}" = rule.yml ]; then finding=1; fi ;;
    esac
}

scan_path() {
    if [ -L "$1" ]; then exit 2; fi
    if [ -f "$1" ]; then
        scan_text "$(cat -- "$1")" "$1"
    elif [ -d "$1" ]; then
        for entry in "$1"/* "$1"/.[!.]* "$1"/..?*; do
            if [ -e "$entry" ] || [ -L "$entry" ]; then scan_path "$entry"; fi
        done
    else
        exit 2
    fi
}

case "$mode" in
  stdin) scan_text "$(cat)" '' ;;
  dir)
    target=${target:-.}
    case "$target" in /*) ;; *) target="$PWD/$target" ;; esac
    case "$target" in "$controls"/tmp/*) scan_path "$target" ;; *) exit 2 ;; esac ;;
  *) exit 2 ;;
esac
if [ "$finding" -eq 1 ]; then emit '[{}]'; exit 1; fi
emit '[]'
"#;

fn oracle() -> bool {
    let Some(path) = std::env::var_os("DOTS_BIN") else {
        return false;
    };
    let path = Path::new(&path);
    assert!(path.is_absolute(), "DOTS_BIN must be absolute");
    let mut header = [0; 2];
    fs::File::open(path)
        .unwrap()
        .read_exact(&mut header)
        .unwrap();
    header == *b"#!"
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn succeeded(output: Output) -> String {
    assert!(
        output.status.success(),
        "status: {}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn excluded(text: &str, forbidden: &[&str]) {
    for word in forbidden {
        assert!(!text.contains(word), "unexpected {word:?} in {text:?}");
    }
}

fn present(text: &str, required: &[&str]) {
    for word in required {
        assert!(text.contains(word), "missing {word:?} in {text:?}");
    }
}

struct ReceiptWorld {
    world: IsolatedWorld,
    root: PathBuf,
    publisher: PathBuf,
    fixture: PathBuf,
    tools: PathBuf,
}

impl ReceiptWorld {
    fn new() -> Self {
        let world = IsolatedWorld::new();
        let root = world.root().to_path_buf();
        let fixture = root.parent().unwrap().join("receipt-fixture");
        let publisher = fixture.join("publisher");
        let tools = fixture.join("bin");
        fs::create_dir_all(&tools).unwrap();
        fs::create_dir(fixture.join("tmp")).unwrap();
        for name in [
            "bash", "cat", "dirname", "git", "grep", "jq", "node", "python3", "sed", "sleep", "tar",
        ] {
            if let Some(path) = executable_on_path(&world.dots(), name) {
                symlink(path, tools.join(name)).unwrap();
            }
        }
        executable(&tools.join("mise"), MISE);
        executable(&tools.join("gitleaks"), SCANNER);

        let f = Self {
            world,
            root,
            publisher,
            fixture,
            tools,
        };
        let remote = f.world.bare_remote("receipt-origin");
        let local_url = format!("file://{}", remote.display());
        let (repository, origin) = if oracle() {
            ("dots-tests/receipt", ORIGIN)
        } else {
            (local_url.as_str(), local_url.as_str())
        };
        f.write(
            &f.root,
            "config.toml",
            format!("[vars]\ndots_profiles = \"test\"\ndots_source_repo = {repository:?}\n"),
        );
        f.git(&f.root, &["init", "-q", "-b", "main"]);
        f.git(&f.root, &["config", "user.name", "Receipt Fixture"]);
        f.git(
            &f.root,
            &["config", "user.email", "receipt@example.invalid"],
        );
        f.write(
            &f.root,
            ".gitignore",
            "/config.local.toml\n/miserc.toml\n/config.host-*.toml\n/mise.lock\n/mise.*.lock\n/.setup-state/\n/locks/\n",
        );
        f.commit(&f.root);
        f.git(&f.root, &["remote", "add", "origin", origin]);
        if oracle() {
            // The frozen dispatcher requires the raw GitHub origin spelling.
            // In the fixture alone, Git resolves it to the temporary bare repo.
            f.git(
                &f.root,
                &["config", &format!("url.{local_url}.insteadOf"), ORIGIN],
            );
        }
        f.git(&f.root, &["push", "-q", "origin", "main"]);
        f.git(&f.root, &["switch", "-qc", "dev"]);
        f.git(&f.root, &["push", "-q", "origin", "dev"]);
        f.git(
            &f.root,
            &[
                "clone",
                "-q",
                "-b",
                "dev",
                remote.to_str().unwrap(),
                f.publisher.to_str().unwrap(),
            ],
        );
        f.git(&f.publisher, &["config", "user.name", "Receipt Fixture"]);
        f.git(
            &f.publisher,
            &["config", "user.email", "receipt@example.invalid"],
        );
        f
    }

    fn isolate(&self, command: &mut FixtureCommand<'_>) {
        command
            .env("PATH", &self.tools)
            .env("TMPDIR", self.fixture.join("tmp"))
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("DOTS_MISE_BIN", self.tools.join("mise"))
            .env("SETUP_PROFILE", "test")
            .env("TERM", "xterm");
    }

    fn git_command(&self, at: &Path, args: &[&str]) -> FixtureCommand<'_> {
        let mut command = self.world.git();
        command.current_dir(at).args(args);
        self.isolate(&mut command);
        command
    }

    fn git(&self, at: &Path, args: &[&str]) -> String {
        succeeded(self.git_command(at, args).output().unwrap())
            .trim()
            .to_owned()
    }

    fn write(&self, at: &Path, name: &str, bytes: impl AsRef<[u8]>) {
        let path = at.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn commit(&self, at: &Path) -> String {
        self.git(at, &["add", "-A"]);
        self.git(at, &["commit", "-qm", "Fixture change"]);
        self.git(at, &["rev-parse", "HEAD"])
    }

    fn publish(&self) -> String {
        let head = self.commit(&self.publisher);
        self.git(&self.publisher, &["push", "-q", "origin", "dev"]);
        head
    }

    fn dots(&self, at: &Path, args: &[&str]) -> FixtureCommand<'_> {
        let mut command = self.world.dots();
        command.args(args);
        command
            .current_dir(at)
            .env("DOTS_ROOT", at)
            .env("MISE_CONFIG_DIR", at);
        self.isolate(&mut command);
        command
    }

    fn sync(&self) -> String {
        succeeded(self.dots(&self.root, &["sync"]).output().unwrap())
    }

    fn last(&self) -> String {
        succeeded(self.dots(&self.root, &["sync", "--last"]).output().unwrap())
    }

    fn prepare_pager(&self) -> PathBuf {
        let capture = self.fixture.join("pager-output");
        executable(
            &self.tools.join("less"),
            "#!/bin/sh\nexec cat > \"${0%/*}/../pager-output\"\n",
        );
        capture
    }

    fn terminal(&self, command: FixtureCommand<'_>) -> String {
        let (status, text) = pty::Pty::spawn(command).finish();
        assert!(status.success(), "{status}: {text}");
        text
    }

    fn replay_to_pager(&self, capture: &Path) -> String {
        if capture.exists() {
            fs::remove_file(capture).unwrap();
        }
        let terminal = self.terminal(self.dots(&self.root, &["sync", "--last"]));
        assert!(
            terminal.trim().is_empty(),
            "unexpected terminal output: {terminal}"
        );
        fs::read_to_string(capture).unwrap()
    }
}

#[test]
#[ignore = "P-09"]
fn unchanged_replay_stays_in_the_selected_checkout() {
    let f = ReceiptWorld::new();
    f.write(&f.publisher, "receipt-routing-path", "incoming\n");
    f.publish();
    f.sync();

    let decoy = f.fixture.join("decoy");
    fs::create_dir(&decoy).unwrap();
    f.git(&decoy, &["init", "-q"]);
    f.git(&decoy, &["config", "user.name", "Receipt Decoy"]);
    f.git(&decoy, &["config", "user.email", "decoy@example.invalid"]);
    f.write(&decoy, "decoy-path", "untouched fixture\n");
    let decoy_head = f.commit(&decoy);
    let output = succeeded(
        f.dots(&f.root, &["sync", "--last"])
            .env("GIT_DIR", decoy.join(".git"))
            .env("GIT_WORK_TREE", &decoy)
            .env("GIT_INDEX_FILE", decoy.join(".git/index"))
            .output()
            .unwrap(),
    );
    present(&output, &["receipt-routing-path"]);
    excluded(&output, &["decoy-path"]);
    assert_eq!(f.git(&decoy, &["rev-parse", "HEAD"]), decoy_head);
    assert!(f.git(&decoy, &["status", "--porcelain"]).is_empty());
}

#[test]
#[ignore = "P-09"]
fn unchanged_replay_lists_renamed_deleted_added_and_binary_paths() {
    let f = ReceiptWorld::new();
    let old_name = "odd\tfile\nname";
    f.write(&f.publisher, old_name, "rename me\n");
    f.write(&f.publisher, "deleted-path", "remove me\n");
    f.write(&f.publisher, "binary-path", [0, 1]);
    f.publish();
    f.sync();

    fs::rename(f.publisher.join(old_name), f.publisher.join("new name")).unwrap();
    fs::remove_file(f.publisher.join("deleted-path")).unwrap();
    f.write(&f.publisher, "binary-path", [0, 2]);
    f.write(&f.publisher, "added-path", "one\ntwo\n");
    let incoming = f.publish();
    // Invalid UTF-8 paths are injected only through the Git index. An
    // executable sync can't check them out on all platforms (notably macOS);
    // the metadata contract permits a path entry without exact Git quoting.
    let output = f.sync();
    let replay = f.last();
    for metadata in [&output, &replay] {
        present(metadata, &["binary-path", "deleted-path", "added-path"]);
        assert!(
            metadata.lines().any(|line| {
                line.contains("new name")
                    && (line.contains(r"odd\tfile\nname")
                        || line.contains(r"odd\u0009file\u000aname"))
            }),
            "missing escaped old and new names on the rename line: {metadata}"
        );
        excluded(metadata, &[old_name]);
    }
    assert_eq!(f.git(&f.root, &["rev-parse", "HEAD"]), incoming);
    excluded(&replay, &[old_name, "remove me", "rename me", "diff --git"]);
}

#[test]
#[ignore = "P-09"]
fn unchanged_clean_sync_is_one_line() {
    let output = ReceiptWorld::new().sync();
    assert_eq!(output.trim().lines().count(), 1, "{output:?}");
}

#[test]
#[ignore = "P-09"]
fn unchanged_sync_limits_each_group_to_five_paths() {
    let f = ReceiptWorld::new();
    for i in 0..6 {
        f.write(&f.root, &format!("local-{i}"), "local fixture body\n");
        f.write(
            &f.publisher,
            &format!("received-{i}"),
            "incoming fixture body\n",
        );
    }
    f.publish();
    let output = f.sync();
    for stem in ["local", "received"] {
        let shown = (0..6)
            .filter(|i| output.contains(&format!("{stem}-{i}")))
            .count();
        assert_eq!(shown, 5, "{output}");
    }
    let replay = f.last();
    present(&replay, &["local-5", "received-5"]);
    excluded(&replay, &["local fixture body", "incoming fixture body"]);
}

#[test]
#[ignore = "P-09"]
fn unchanged_latest_receipt_is_checkout_local() {
    let f = ReceiptWorld::new();
    f.write(
        &f.publisher,
        "initial-receipt-entry",
        "initial fixture body\n",
    );
    f.publish();
    f.sync();
    present(&f.last(), &["initial-receipt-entry"]);

    let linked = f.fixture.join("linked");
    f.git(
        &f.root,
        &["worktree", "add", "-qb", "other", linked.to_str().unwrap()],
    );
    let no_receipt = f.dots(&linked, &["sync", "--last"]).output().unwrap();
    let empty = format!(
        "{}{}",
        String::from_utf8_lossy(&no_receipt.stdout),
        String::from_utf8_lossy(&no_receipt.stderr),
    );
    assert!(
        empty.to_ascii_lowercase().contains("no sync receipt"),
        "{empty}"
    );
    let stopped = succeeded(f.dots(&linked, &["sync"]).output().unwrap());
    present(&stopped.to_ascii_lowercase(), &["other", "dev"]);
    let linked_last = succeeded(f.dots(&linked, &["sync", "--last"]).output().unwrap());
    present(&linked_last.to_ascii_lowercase(), &["other"]);
    excluded(&linked_last, &["initial-receipt-entry"]);

    f.write(
        &f.publisher,
        "latest-receipt-entry",
        "payload remains in Git\n",
    );
    let endpoint = f.publish();
    f.sync();
    excluded(
        &f.last(),
        &["initial-receipt-entry", "payload remains in Git"],
    );
    present(&f.last(), &["latest-receipt-entry"]);
    let capture = f.prepare_pager();
    let before_prune = f.replay_to_pager(&capture);
    present(
        &before_prune,
        &["latest-receipt-entry", "+payload remains in Git"],
    );
    // An unreachable/deleted revision is a valid reason for replay to omit
    // a patch. The endpoint is a loose object made by this fixture alone.
    let object = f
        .root
        .join(".git/objects")
        .join(&endpoint[..2])
        .join(&endpoint[2..]);
    fs::remove_file(object).unwrap();
    fs::remove_file(&capture).unwrap();
    let terminal = f.terminal(f.dots(&f.root, &["sync", "--last"]));
    let pager = if capture.is_file() {
        fs::read_to_string(capture).unwrap()
    } else {
        String::new()
    };
    let replay = format!("{terminal}{pager}");
    present(&replay, &["latest-receipt-entry"]);
    excluded(&replay, &["payload remains in Git"]);
}

#[test]
#[ignore = "P-09"]
fn new_replay_ignores_a_pre_1_0_receipt() {
    let f = ReceiptWorld::new();
    // Supply legacy input to a valid checkout that has never run sync. No
    // assumptions about where the current implementation saves its data.
    let fixture = r#"{"version":1,"finishedAt":"2026-01-01T00:00:00.000Z","source":"sync","status":"complete","scanBlocked":false,"issues":[{"summary":"legacy receipt only","detail":"","action":""}],"notices":[],"groups":[]}"#;
    let file = f.root.join(".git/dots-sync-receipt.json");
    fs::write(&file, fixture).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let output = f.dots(&f.root, &["sync", "--last"]).output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    excluded(&text, &["legacy receipt only"]);
    assert!(
        text.to_ascii_lowercase().contains("no sync receipt")
            || text.to_ascii_lowercase().contains("no receipt"),
        "{text}"
    );
}

#[test]
#[ignore = "P-09"]
fn unchanged_inline_patch_uses_committed_inputs() {
    let f = ReceiptWorld::new();
    f.write(&f.publisher, ".gitattributes", "tracked-text diff=marker\n");
    f.write(
        &f.publisher,
        "tracked-text",
        "first\noriginal receipt line\nlast\n",
    );
    let old = f.publish();
    f.sync();

    f.write(
        &f.publisher,
        "tracked-text",
        "first\nupdated receipt line\nlast\n",
    );
    let new = f.publish();
    let live = f.terminal(f.dots(&f.root, &["sync"]));
    present(&live, &["-original receipt line", "+updated receipt line"]);
    assert!(live.trim().lines().count() <= 12, "{live}");

    f.write(&f.root, "tracked-text", "uncommitted receipt text\n");
    let alternative = f.commit(&f.root);
    f.git(&f.root, &["replace", &old, &alternative]);
    f.git(&f.root, &["replace", &new, &alternative]);
    f.write(
        &f.root,
        "tracked-text",
        "uncommitted receipt text\nsecond line\n",
    );
    f.write(&f.root, ".gitattributes", "tracked-text -diff\n");

    let marker = f.fixture.join("external-diff-was-run");
    let driver = f.tools.join("fixture-external-diff");
    executable(
        &driver,
        "#!/bin/sh\nprintf 'invoked' > \"${0%/*}/../external-diff-was-run\"\n",
    );
    for key in [
        "diff.external",
        "diff.marker.command",
        "diff.marker.textconv",
    ] {
        f.git(&f.root, &["config", key, driver.to_str().unwrap()]);
    }

    let capture = f.prepare_pager();
    let replay = f.replay_to_pager(&capture);
    present(
        &replay,
        &["-original receipt line", "+updated receipt line"],
    );
    excluded(&replay, &["uncommitted receipt text"]);
    assert!(!marker.exists());
}

#[test]
#[ignore = "P-09"]
fn unchanged_inline_suppresses_overlong_patch() {
    let f = ReceiptWorld::new();
    f.write(
        &f.publisher,
        "long-patch-path",
        "body outside the inline limit\n".repeat(16),
    );
    f.publish();
    let live = f.terminal(f.dots(&f.root, &["sync"]));
    present(&live, &["long-patch-path"]);
    excluded(&live, &["body outside the inline limit"]);
}

#[test]
#[ignore = "P-09"]
fn unchanged_inline_at_most_two_paths() {
    let f = ReceiptWorld::new();
    for name in ["first", "second"] {
        f.write(&f.publisher, name, format!("short {name} fixture\n"));
    }
    f.publish();
    let live = f.terminal(f.dots(&f.root, &["sync"]));
    present(&live, &["+short first fixture", "+short second fixture"]);
    assert!(live.trim().lines().count() <= 12, "{live}");

    for name in ["first", "second", "third"] {
        f.write(&f.publisher, name, format!("three path {name} fixture\n"));
    }
    f.publish();
    let live = f.terminal(f.dots(&f.root, &["sync"]));
    present(&live, &["first", "second", "third"]);
    excluded(&live, &["three path"]);
}

#[test]
#[ignore = "P-09"]
fn unchanged_plain_replay_has_metadata_without_pager() {
    let f = ReceiptWorld::new();
    f.write(
        &f.publisher,
        "plain-receipt-path",
        "plain never shows body\n",
    );
    f.publish();
    f.sync();
    let capture = f.prepare_pager();
    let replay = f.terminal(f.dots(&f.root, &["--plain", "sync", "--last"]));
    present(&replay, &["plain-receipt-path"]);
    excluded(&replay, &["plain never shows body"]);
    assert!(!capture.exists());
}

#[test]
#[ignore = "P-09"]
fn unchanged_live_terminal_omits_a_displayable_secret_patch() {
    let f = ReceiptWorld::new();
    let file = "live-screening.txt";
    f.write(&f.publisher, file, "safe inline control\n");
    f.publish();
    let control = f.terminal(f.dots(&f.root, &["sync"]));
    present(&control, &["+safe inline control"]);

    let canary = "SCAN_ALERT synthetic live secret";
    f.write(&f.publisher, file, format!("{canary}\n"));
    f.publish();
    let scans = f.fixture.join("scanned-content");
    fs::write(&scans, "").unwrap();
    let live = f.terminal(f.dots(&f.root, &["sync"]));
    present(&live, &[file]);
    excluded(&live, &[canary, "-safe inline control"]);
    present(&fs::read_to_string(scans).unwrap(), &[canary]);
}

#[test]
#[ignore = "P-09"]
fn unchanged_replay_screens_both_file_versions_and_displayed_text() {
    let f = ReceiptWorld::new();
    let old = "PATH_RULE old path-sensitive bytes";
    let new = "PATH_RULE new path-sensitive bytes";
    f.write(&f.publisher, "01-before/rule.yml", format!("{old}\n"));
    f.publish();
    f.sync();

    f.write(&f.publisher, "00-safe-path", "safe receipt body\n");
    f.write(&f.publisher, "01-before/rule.yml", "new safe path data\n");
    f.write(&f.publisher, "02-after/rule.yml", format!("{new}\n"));
    f.write(
        &f.publisher,
        "03-display-finding",
        "DISPLAY_ONLY rendered bytes\n",
    );
    f.publish();
    f.sync();

    let capture = f.prepare_pager();
    let scans = f.fixture.join("scanned-content");
    fs::write(&scans, "").unwrap();
    let replay = f.replay_to_pager(&capture);
    present(
        &replay,
        &[
            "00-safe-path",
            "01-before/rule.yml",
            "02-after/rule.yml",
            "03-display-finding",
            "+safe receipt body",
        ],
    );
    let events = fs::read_to_string(scans).unwrap();
    present(
        &events,
        &[
            &format!("dir {old}"),
            &format!("dir {new}"),
            "+DISPLAY_ONLY rendered bytes",
        ],
    );
    let displayed = events
        .split("stdin ")
        .skip(1)
        .filter_map(|event| event.split("dir ").next())
        .any(|event| event.contains("+DISPLAY_ONLY rendered bytes"));
    assert!(
        displayed,
        "displayed bytes did not reach scanner stdin: {events}"
    );
    excluded(
        &replay,
        &["PATH_RULE", "new safe path data", "DISPLAY_ONLY"],
    );

    let control = f.fixture.join("scanner-failure");
    for failure in ["error", "empty", "malformed"] {
        fs::write(&control, failure).unwrap();
        let replay = f.replay_to_pager(&capture);
        present(&replay, &["safe-path"]);
        excluded(&replay, &["safe receipt body"]);
    }
    fs::remove_file(control).unwrap();

    fs::remove_file(f.tools.join("gitleaks")).unwrap();
    let replay = f.replay_to_pager(&capture);
    present(&replay, &["safe-path"]);
    excluded(&replay, &["safe receipt body"]);
}

#[test]
#[ignore = "P-09"]
fn unchanged_replay_omits_unsafe_patch_bodies() {
    let f = ReceiptWorld::new();
    f.write(&f.publisher, "removed-path", "SCAN_ALERT removed bytes\n");
    f.write(
        &f.publisher,
        "context-path",
        "SCAN_ALERT unchanged context\nold fixture\n",
    );
    f.write(&f.publisher, "binary-path", [0, 1]);
    f.write(
        &f.publisher,
        "encrypted.age",
        "encrypted fixture placeholder\n",
    );
    f.publish();
    f.sync();

    fs::remove_file(f.publisher.join("removed-path")).unwrap();
    fs::remove_file(f.publisher.join("encrypted.age")).unwrap();
    f.write(
        &f.publisher,
        "context-path",
        "SCAN_ALERT unchanged context\nnew fixture\n",
    );
    f.write(&f.publisher, "binary-path", [0, 2]);
    f.write(&f.publisher, "added-finding", "SCAN_ALERT incoming bytes\n");
    f.write(&f.publisher, "ciphertext-header", "age-encryption.org/v1\n");
    f.write(&f.publisher, "escape-byte", b"escape \x1b[31m payload\n");
    f.write(&f.publisher, "bidi-byte", "bidi \u{202e} payload\n");
    f.write(&f.publisher, "invalid-utf8", [0xff, b'\n']);
    let unsafe_name = "00-odd\nname";
    f.write(&f.publisher, unsafe_name, "unsafe filename fixture\n");
    f.publish();
    f.sync();

    let capture = f.prepare_pager();
    let replay = f.replay_to_pager(&capture);
    assert!(
        replay.contains(r"00-odd\nname") || replay.contains(r"00-odd\u000aname"),
        "{replay}"
    );
    present(
        &replay,
        &[
            "removed-path",
            "context-path",
            "binary-path",
            "encrypted.age",
            "added-finding",
            "ciphertext-header",
            "escape-byte",
            "bidi-byte",
            "invalid-utf8",
        ],
    );
    excluded(
        &replay,
        &[
            "SCAN_ALERT",
            "age-encryption.org/v1",
            "payload",
            "unsafe filename fixture",
            unsafe_name,
            "encrypted fixture placeholder",
        ],
    );
}

#[test]
#[ignore = "P-09"]
fn unchanged_replay_caps_retained_patch_bytes() {
    let f = ReceiptWorld::new();
    // Distinct local and incoming paths make independently scoped receipt
    // groups; their combined patch bodies alone exceed the pager limit.
    let large = format!("{}\n", "x".repeat(60_000));
    for i in 0..10 {
        f.write(&f.root, &format!("local-large-{i:02}"), &large);
        f.write(&f.publisher, &format!("incoming-large-{i:02}"), &large);
    }
    f.write(&f.root, "00-small", "small screened body\n");
    f.publish();
    f.sync();

    let capture = f.prepare_pager();
    let replay = f.replay_to_pager(&capture);
    present(
        &replay,
        &[
            "local-large-09",
            "incoming-large-09",
            "+small screened body",
        ],
    );
    let long_lines = replay.lines().filter(|line| {
        let bytes = line.trim_start().as_bytes();
        bytes.len() > 1000 && bytes.starts_with(b"+xxxxx")
    });
    let retained = long_lines.map(str::len).sum::<usize>();
    assert!(
        retained > 0 && retained <= 1024 * 1024,
        "retained {retained} patch bytes"
    );
}

#[test]
#[ignore = "P-09"]
fn unchanged_replay_keeps_screened_patches_when_screening_stalls() {
    let f = ReceiptWorld::new();
    f.write(&f.publisher, "a-clean", "screened before the stall\n");
    f.write(&f.publisher, "m-stalled", "STALL_SCAN demand\n");
    f.write(&f.publisher, "z-later", "unscreened after the stall\n");
    f.publish();
    let sync = f.sync();

    f.write(&f.root, "config.toml", "broken = ???\n");
    let no_mise = f.fixture.join("mise-was-called");
    executable(
        &f.tools.join("mise"),
        "#!/bin/sh\nprintf 'invoked' > \"${0%/*}/../mise-was-called\"\nexit 17\n",
    );
    let capture = f.prepare_pager();
    let stall = f.fixture.join("scanner-stalled");
    let scans = f.fixture.join("scanned-content");
    fs::write(&scans, "").unwrap();
    let started = Instant::now();
    let replay = f.replay_to_pager(&capture);
    assert!(
        started.elapsed().as_secs() < 8,
        "stalled scan did not stop promptly"
    );
    let scanned = fs::read_to_string(scans).unwrap();
    assert!(
        stall.exists(),
        "the fixture never reached the stalled scan:\n{sync}\n{replay}\nScanned:\n{scanned}"
    );
    assert!(!no_mise.exists(), "replay invoked mise");
    present(
        &replay,
        &[
            "a-clean",
            "m-stalled",
            "z-later",
            "+screened before the stall",
        ],
    );
    excluded(
        &replay,
        &["STALL_SCAN demand", "unscreened after the stall"],
    );
    excluded(&scanned, &["unscreened after the stall"]);
}
