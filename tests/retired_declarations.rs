#[path = "p07/isolation.rs"]
mod isolation;
mod support;

use isolation::{IsolatedWorld, executable_on_path, fixture_file};
use std::fs;
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::{env, str};
use support::FixtureCommand;

const SOURCE: &str = "templates/herdr.tera";
const ROW: &str = "dotfiles.herdr\ttemplates/herdr.tera\n";
const BEFORE: &str = "# this machine\n[env]\nFIXTURE_ROLE = \"laptop\"\n";
const DEPLOYED: &str = "deployed template bytes\n";
const IGNORE: &str = "/config.local.toml\n/config.host-*.toml\n/miserc.toml\n/mise.lock\n/mise.*.lock\n/.setup-state/\n/locks/\n";
const CLEAN_SCAN: &str = r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
    case "$1" in
        --report-path) shift; printf '[]\n' > "$1" ;;
        --report-path=*) printf '[]\n' > "${1#*=}" ;;
    esac
    shift
done
"#;

struct Retirement {
    world: IsolatedWorld,
    root: PathBuf,
    incoming: String,
    path: std::ffi::OsString,
}

impl Retirement {
    fn new(rows: &str, keep_source: bool) -> Self {
        Self::with_source(rows, keep_source, None)
    }

    fn with_source(rows: &str, keep_source: bool, extra_source: Option<&str>) -> Self {
        let world = IsolatedWorld::new();
        let base = world.root().parent().unwrap();
        let bin = base.join("retirement-bin");
        let root = base.join("retirement");
        let peer = base.join("peer");
        for dir in [&bin, &root, &base.join("tmp"), &base.join("mise-system")] {
            fs::create_dir_all(dir).unwrap();
        }
        let mut prefix = [0; 2];
        fs::File::open(world.dots().get_program())
            .unwrap()
            .read_exact(&mut prefix)
            .unwrap();
        let oracle = prefix == *b"#!";
        let programs: &[&str] = if oracle {
            &["git", "mise", "node", "jq", "python3"]
        } else {
            &["git", "mise"]
        };
        for program in programs {
            let executable = executable_on_path(&world.dots(), program).unwrap_or_else(|| {
                panic!("{program} must be installed on PATH for the retirement fixture")
            });
            let executable = if *program == "mise" {
                world.native_mise_adapter(&executable)
            } else {
                executable
            };
            symlink(executable, bin.join(program)).unwrap();
        }
        fs::write(bin.join("gitleaks"), CLEAN_SCAN).unwrap();
        fs::set_permissions(bin.join("gitleaks"), fs::Permissions::from_mode(0o755)).unwrap();
        let path =
            env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
        let remote = world.bare_remote("retirement.git");
        let remote_url = format!("file://{}", remote.display());
        let repository = if oracle {
            "fixture-owner/fixture-repo"
        } else {
            &remote_url
        };
        let mut fixture = Self {
            world,
            root,
            incoming: String::new(),
            path,
        };

        fixture.git(&fixture.root, ["init", "--initial-branch=dev"]);
        fixture.write(
            "config.toml",
            &format!("[vars]\ndots_profiles = \"laptop\"\ndots_source_repo = \"{repository}\"\n"),
        );
        fixture.write(".gitignore", IGNORE);
        fixture.write(SOURCE, "theme = \"fixture\"\n");
        fixture.write("dotfiles/other", "valid alternative source\n");
        fixture.git(&fixture.root, ["add", "."]);
        fixture.git(&fixture.root, ["commit", "-m", "Seed"]);
        fixture.git(&fixture.root, ["push", &remote_url, "dev:main", "dev:dev"]);
        if oracle {
            // The oracle requires a GitHub-shaped configured URL. Only this
            // fixture's local Git config maps it to its own bare repository.
            let github = "https://github.com/fixture-owner/fixture-repo";
            fixture.git(&fixture.root, ["remote", "add", "origin", github]);
            fixture.git(
                &fixture.root,
                [
                    "config",
                    "--local",
                    &format!("url.{remote_url}.insteadOf"),
                    github,
                ],
            );
        } else {
            fixture.git(&fixture.root, ["remote", "add", "origin", &remote_url]);
        }
        fixture.git(
            &fixture.root,
            [
                "clone",
                "--branch",
                "dev",
                &remote_url,
                peer.to_str().unwrap(),
            ],
        );
        fs::create_dir(peer.join("dots")).unwrap();
        fs::write(peer.join("dots/retired-declarations"), rows).unwrap();
        if !keep_source {
            fixture.git(&peer, ["rm", SOURCE]);
        }
        if let Some(source) = extra_source {
            let path = peer.join(source);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "valid fixture source\n").unwrap();
        }
        fixture.git(&peer, ["add", "."]);
        fixture.git(&peer, ["commit", "-m", "Update retirement list"]);
        fixture.git(&peer, ["push", "origin", "dev"]);
        fixture.incoming = fixture.git(&peer, ["rev-parse", "HEAD"]);
        fs::write(fixture.world.home().join(".retired-fixture"), DEPLOYED).unwrap();
        fixture
    }

    fn git<const N: usize>(&self, cwd: &Path, args: [&str; N]) -> String {
        let mut cmd = self.world.git();
        cmd.current_dir(cwd).args(args);
        self.isolate_tools(&mut cmd);
        let output = success(&mut cmd);
        str::from_utf8(&output.stdout).unwrap().trim().to_owned()
    }

    fn isolate_tools(&self, cmd: &mut FixtureCommand<'_>) {
        let base = self.root.parent().unwrap();
        cmd.env("PATH", &self.path)
            .env("GIT_ALLOW_PROTOCOL", "file")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("MISE_OFFLINE", "1")
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_AUTO_UPDATE", "0")
            .env("MISE_NOT_FOUND_AUTO_INSTALL", "0")
            .env("MISE_NO_HOOKS", "1")
            .env("MISE_SYSTEM_CONFIG_DIR", base.join("mise-system"))
            .env("MISE_CEILING_PATHS", base)
            .env("MISE_TRUSTED_CONFIG_PATHS", base)
            .env("TMPDIR", base.join("tmp"));
    }

    fn write(&self, file: &str, text: &str) {
        let path = self.root.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn set_local(&self, local: &str) {
        self.write("config.local.toml", local);
        fs::set_permissions(
            self.root.join("config.local.toml"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    fn sync_command(&self) -> FixtureCommand<'_> {
        let mut cmd = self.world.dots();
        cmd.current_dir(&self.root)
            .env("DOTS_ROOT", &self.root)
            .env("MISE_CONFIG_DIR", &self.root)
            .arg("sync");
        self.isolate_tools(&mut cmd);
        cmd
    }

    fn sync(&self) -> String {
        let output = success(&mut self.sync_command());
        format!(
            "{}{}",
            str::from_utf8(&output.stdout).unwrap(),
            str::from_utf8(&output.stderr).unwrap()
        )
    }

    fn assert_local(&self, expected: &str) {
        let local = fixture_file(
            self.root.parent().unwrap(),
            &self.root.join("config.local.toml"),
        )
        .expect("live configuration must remain a regular fixture file");
        assert_eq!(
            local.file.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::io::read_to_string(local.file).unwrap(), expected);
    }

    fn assert_deployed(&self) {
        let deployed = self.world.home().join(".retired-fixture");
        assert!(deployed.symlink_metadata().unwrap().is_file());
        assert_eq!(fs::read_to_string(deployed).unwrap(), DEPLOYED);
    }

    fn assert_advanced(&self) {
        assert_eq!(self.git(&self.root, ["rev-parse", "HEAD"]), self.incoming);
    }
}

fn success(cmd: &mut FixtureCommand<'_>) -> Output {
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "{:?}:\n{}\n{}",
        **cmd,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn declaration(header: &str, source_assignment: &str) -> String {
    format!("{header}\n{source_assignment}\nmode = \"template\"\ntarget = \"~/.retired-fixture\"\n")
}

fn table() -> String {
    declaration("[dotfiles.herdr]", &format!("source = \"{SOURCE}\""))
}

fn assert_retired(local: &str, expected: &str, rows: &str) {
    let fixture = Retirement::new(rows, false);
    fixture.set_local(local);
    let output = fixture.sync();
    fixture.assert_advanced();
    fixture.assert_local(expected);
    fixture.assert_deployed();
    assert!(!fixture.root.join(SOURCE).exists());
    // The backup path is printed; its naming and location are internal.
    let saved = output
        .split_whitespace()
        .map(|word| word.trim_matches(|c| matches!(c, '\'' | '"' | '`' | ')')))
        .map(|word| fixture.root.join(word))
        .filter(|path| path != &fixture.root.join("config.local.toml"))
        .filter_map(|path| fixture_file(fixture.root.parent().unwrap(), &path))
        .filter(|entry| entry.file.metadata().unwrap().permissions().mode() & 0o777 == 0o600)
        .any(|entry| std::io::read_to_string(entry.file).is_ok_and(|bytes| bytes == local));
    assert!(
        saved,
        "missing printed private backup of the original local file: {output}"
    );
}

fn assert_kept(local: &str, rows: &str, keep_source: bool) {
    assert_kept_with_source(local, rows, keep_source, None);
}

fn has_exact_file_name(path: &Path) -> bool {
    let mut entries = match fs::read_dir(path.parent().unwrap()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
        Err(error) => panic!("could not inspect fixture source {path:?}: {error}"),
    };
    let name = path.file_name().unwrap();
    // A path lookup can resolve a differently spelled entry on macOS.
    entries.any(|entry| {
        let entry = entry.unwrap();
        entry.file_name() == name && entry.file_type().unwrap().is_file()
    })
}

fn assert_kept_with_source(local: &str, rows: &str, keep_source: bool, extra_source: Option<&str>) {
    let fixture = Retirement::with_source(rows, keep_source, extra_source);
    fixture.set_local(local);
    fixture.sync();
    fixture.assert_advanced();
    fixture.assert_local(local);
    assert_eq!(has_exact_file_name(&fixture.root.join(SOURCE)), keep_source);
    if let Some(source) = extra_source {
        assert!(
            has_exact_file_name(&fixture.root.join(source)),
            "missing fixture source with exact spelling {source:?}"
        );
    }
}

#[test]
#[ignore = "P-08"]
fn unchanged_exact_retired_table_is_removed_byte_for_byte() {
    let after = "\n# following comment\n[settings]\nexperimental = false\n";
    assert_retired(&format!("{BEFORE}\n{}", table()), BEFORE, ROW);
    assert_retired(
        &format!("{BEFORE}\n{}{after}", table()),
        &format!("{BEFORE}{after}"),
        ROW,
    );
    assert_retired(&format!("{}\n{BEFORE}", table()), BEFORE, ROW);
    let crlf = format!("{BEFORE}\n{}{}", table(), after.trim_end()).replace('\n', "\r\n");
    let expected = format!("{BEFORE}{}", after.trim_end()).replace('\n', "\r\n");
    assert_retired(&crlf, &expected, ROW);
}

#[test]
#[ignore = "P-08"]
fn unchanged_quoted_table_and_source_match() {
    for header in [
        "[dotfiles.\"herdr\"]",
        "[ 'dotfiles' . herdr ] # old",
        "[dotfiles.herdr] # appended",
    ] {
        let table = declaration(header, &format!("source = '{SOURCE}' # template"));
        assert_retired(&format!("{BEFORE}\n{table}"), BEFORE, ROW);
    }
    let escaped = declaration(
        "[dotfiles.herdr]",
        r#"source = "templates\u002fherdr.tera""#,
    );
    assert_retired(&format!("{BEFORE}\n{escaped}"), BEFORE, ROW);
}

#[test]
#[ignore = "P-08"]
fn unchanged_different_source_keeps_declaration() {
    assert_kept(
        &format!("{BEFORE}\n{}", table().replace(SOURCE, "dotfiles/other")),
        ROW,
        false,
    );
}

#[test]
#[ignore = "P-08"]
fn unchanged_source_present_in_the_merge_keeps_declaration() {
    assert_kept(&format!("{BEFORE}\n{}", table()), ROW, true);
}

#[test]
#[ignore = "P-08"]
fn unchanged_similar_and_commented_headers_stay() {
    let similarly_named = table().replace("[dotfiles.herdr]", "[dotfiles.herdr-extra]");
    assert_kept(&format!("{BEFORE}\n{similarly_named}"), ROW, false);
    let commented = table().replace("[dotfiles.herdr]", "# [dotfiles.herdr]");
    assert_kept(&format!("{BEFORE}\n{commented}"), ROW, false);
    let neighbour = table()
        .replace("[dotfiles.herdr]", "[dotfiles.herdr-extra]")
        .replace(SOURCE, "dotfiles/other");
    assert_retired(
        &format!("{BEFORE}\n{}\n{neighbour}", table()),
        &format!("{BEFORE}\n{neighbour}"),
        ROW,
    );
}

#[test]
#[ignore = "P-08"]
fn unchanged_nested_definition_keeps_local_declaration() {
    assert_kept(
        &format!("{BEFORE}\n{}\n[dotfiles.herdr.extra]\nx = 1\n", table()),
        ROW,
        false,
    );
}

#[test]
#[ignore = "P-08"]
fn unchanged_string_lookalike_stays_and_array_prefix_is_preserved() {
    let quoted = format!("[vars]\nfixture_heredoc = '''\n{}'''\n", table());
    assert_kept(&quoted, ROW, false);
    let prefix = "[settings]\ndisable_tools = [\n  \"fixture-tool\",\n]\n";
    assert_retired(&format!("{prefix}\n{}", table()), prefix, ROW);
}

#[test]
#[ignore = "P-08"]
fn unchanged_retirement_is_idempotent() {
    let fixture = Retirement::new(ROW, false);
    fixture.set_local(&format!("{BEFORE}\n{}", table()));
    fixture.sync();
    fixture.assert_advanced();
    fixture.assert_local(BEFORE);
    let first = fs::metadata(fixture.root.join("config.local.toml"))
        .unwrap()
        .modified()
        .unwrap();
    fixture.sync();
    fixture.assert_local(BEFORE);
    fixture.assert_deployed();
    assert_eq!(
        fs::metadata(fixture.root.join("config.local.toml"))
            .unwrap()
            .modified()
            .unwrap(),
        first
    );
}

#[test]
#[ignore = "P-08"]
fn unchanged_list_accepts_only_two_field_dotfile_rows() {
    let quoted_row = format!("# comment\n\n dotfiles . \"her.dr\" \t{SOURCE}\r\n");
    let quoted = table().replace("[dotfiles.herdr]", "[dotfiles.\"her.dr\"]");
    assert_retired(&format!("{BEFORE}\n{quoted}"), BEFORE, &quoted_row);
    let with_space = table().replace("[dotfiles.herdr]", "[dotfiles.\"a b\"]");
    assert_retired(
        &format!("{BEFORE}\n{with_space}"),
        BEFORE,
        &format!("dotfiles.\"a b\"\t{SOURCE}\n"),
    );
    // The same source disappears in both fixtures. Only the valid row
    // may remove the declaration.
    let exact = format!("{BEFORE}\n{}", table());
    assert_kept(&exact, &format!("dotfiles.herdr\t{SOURCE}\textra\n"), false);
    assert_retired(&exact, BEFORE, ROW);
}

#[test]
#[ignore = "P-08"]
fn unchanged_list_rejects_noncanonical_paths() {
    for path in [
        "templates/./herdr.tera",
        "./templates/herdr.tera",
        "templates//herdr.tera",
        "templates/../templates/herdr.tera",
    ] {
        let source = format!("'{}'", path);
        let matching = declaration("[dotfiles.herdr]", &format!("source = {source}"));
        assert_kept(
            &format!("{BEFORE}\n{matching}"),
            &format!("dotfiles.herdr\t{path}\n"),
            false,
        );
    }
    let hidden = "templates/.herdr/a..b.tera";
    let matching = table().replace(SOURCE, hidden);
    assert_retired(
        &format!("{BEFORE}\n{matching}"),
        BEFORE,
        &format!("dotfiles.herdr\t{hidden}\n"),
    );
}

#[test]
#[ignore = "P-08"]
fn unchanged_local_source_is_compared_without_path_normalization() {
    for local in ["templates/HERDR.tera", "templates/herdr.tera.extra"] {
        let matching = declaration("[dotfiles.herdr]", &format!("source = '{local}'"));
        assert_kept_with_source(&format!("{BEFORE}\n{matching}"), ROW, false, Some(local));
    }
}

#[test]
#[ignore = "P-08"]
fn new_multiline_toml_source_matches_decoded_value() {
    for quote in ["\"\"\"", "'''"] {
        let table = declaration(
            "[dotfiles.herdr]",
            &format!("source = {quote}{SOURCE}{quote}"),
        );
        assert_retired(&format!("{BEFORE}\n{table}"), BEFORE, ROW);
    }
}
