use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use std::{fs, thread};

use nix::libc;
use toml_edit::{DocumentMut, value};

use super::child::{self, Budget};
use super::{ChangeGroup, Receipt, Tools, path_label, terminal_control};
use crate::Env;

pub(super) const MAX_BYTES: usize = 1024 * 1024;
const SCREEN_BUDGET: Duration = Duration::from_secs(3);
const OMISSION_BYTES: usize = 72;

pub(super) fn render(
    env: &Env,
    root: &Path,
    receipt: &Receipt,
    replay: bool,
    bytes: usize,
) -> Screened {
    render_until(
        env,
        root,
        receipt,
        replay,
        bytes,
        Instant::now() + SCREEN_BUDGET,
    )
}

pub(super) fn render_until(
    env: &Env,
    root: &Path,
    receipt: &Receipt,
    replay: bool,
    bytes: usize,
    deadline: Instant,
) -> Screened {
    let groups: Vec<_> = receipt
        .groups()
        .into_iter()
        .flat_map(|(_, groups)| groups)
        .filter(|group| !group.paths.is_empty())
        .cloned()
        .collect();
    let candidates = groups.iter().map(|group| group.paths.len()).sum::<usize>();
    if candidates == 0 || bytes < OMISSION_BYTES || (!replay && candidates > 2) {
        return Screened::default();
    }

    let mut screened = Screened::default();
    if receipt.checkout.as_deref() == Some(root) {
        let root = root.to_owned();
        let env = Env {
            vars: env.vars.clone(),
            cwd: env.cwd().to_owned(),
        };
        screened = collect_until(deadline, move |budget, send| {
            screen(&env, &root, &groups, replay, bytes, budget, send);
        });
    }
    let omitted = candidates - screened.patches;
    if replay && omitted != 0 {
        let files = if omitted == 1 { "file" } else { "files" };
        let _ = writeln!(screened.text, "\nPatch omitted for {omitted} {files}.");
    }
    screened
}

#[derive(Default)]
pub(super) struct Screened {
    pub(super) text: String,
    patches: usize,
    worker: Option<thread::JoinHandle<()>>,
}

impl Drop for Screened {
    fn drop(&mut self) {
        // Callers retain this guard until after publishing the receipt. The
        // worker alone owns scratch and cannot race cleanup with staging.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn collect_until(
    deadline: Instant,
    work: impl FnOnce(&Budget, mpsc::Sender<String>) + Send + 'static,
) -> Screened {
    let budget = Arc::new(Budget::new(deadline));
    let (send, receive) = mpsc::channel();
    let worker_budget = Arc::clone(&budget);
    let worker = thread::Builder::new()
        .spawn(move || work(&worker_budget, send))
        .ok();
    let mut text = String::new();
    let mut patches = 0;
    if worker.is_some() {
        while let Ok(patch) = receive.recv_timeout(budget.remaining()) {
            patches += 1;
            text.push_str(&patch);
        }
        budget.cancel();
    }
    Screened {
        text,
        patches,
        worker,
    }
}

fn screen(
    env: &Env,
    root: &Path,
    groups: &[ChangeGroup],
    replay: bool,
    bytes: usize,
    budget: &Budget,
    send: mpsc::Sender<String>,
) {
    let Ok(dir) = private_directory().tempdir() else {
        return;
    };
    let tools = Tools::new(env).with_scratch_home(dir.path());
    if run(
        git(&tools, root).args(["rev-parse", "--is-inside-work-tree", "--show-prefix"]),
        budget,
    )
    .as_deref()
        != Some(b"true\n\n")
        || budget.expired()
    {
        return;
    }
    let gitleaks = crate::process::tool_path(env, "gitleaks", false, |args| {
        let mise = crate::process::mise_binary(env).ok()?;
        let managed = crate::config::Root::from_dir(env, root).ok()?;
        let selected = crate::config::selection::read(&managed, env).ok()?;
        let mut command = crate::process::quiet_probe(&mise, root, dir.path(), env);
        match env.get("MISE_STATE_DIR") {
            Some(state) => command.env("MISE_STATE_DIR", state),
            None => command.env_remove("MISE_STATE_DIR"),
        };
        run(
            command
                .env("MISE_ENV", selected.active_environments())
                .args(args),
            budget,
        )
    });
    let Some(scanner) = Scanner::new(&tools, &gitleaks, root, dir.path(), budget) else {
        return;
    };
    let mut lines = 0;
    let mut used = 0;
    for group in groups {
        for path in &group.paths {
            if budget.expired() {
                return;
            }
            let Some(patch) = prepare(&tools, root, group, path, budget) else {
                continue;
            };
            let patch_lines = patch.text.lines().count();
            let extra = if replay { OMISSION_BYTES } else { 0 };
            if used + patch.text.len() + extra > bytes || (!replay && lines + patch_lines > 12) {
                continue;
            }
            if scanner.clean(path, &patch, budget) && !budget.expired() {
                lines += patch_lines;
                used += patch.text.len();
                if send.send(patch.text).is_err() {
                    return;
                }
            }
        }
    }
}

fn private_directory() -> tempfile::Builder<'static, 'static> {
    let mut builder = tempfile::Builder::new();
    builder.permissions(fs::Permissions::from_mode(0o700));
    builder
}

fn private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?
        .write_all(bytes)
}

struct Patch {
    before: Vec<u8>,
    after: Vec<u8>,
    text: String,
}

fn prepare(
    tools: &Tools,
    root: &Path,
    group: &ChangeGroup,
    path: &Path,
    budget: &Budget,
) -> Option<Patch> {
    if !relative(path)
        || private_path(path)
        || encrypted_name(path)
        || !path
            .to_str()
            .is_some_and(|name| !name.chars().any(terminal_control))
    {
        return None;
    }
    let before = commit(group.before.as_deref()?)?;
    let after = commit(group.after.as_deref()?)?;
    let (old, new) = thread::scope(|scope| {
        let old = scope.spawn(|| blob(tools, root, &before, path, budget));
        let new = blob(tools, root, &after, path, budget);
        (old.join().ok().flatten(), new)
    });
    let old = old?;
    let new = new?;
    if old == new {
        return None;
    }
    let text = change_text(path, text(&old)?, text(&new)?)?;
    Some(Patch {
        before: old,
        after: new,
        text,
    })
}

fn change_text(path: &Path, old: &str, new: &str) -> Option<String> {
    let encrypted = [
        "age-encryption.org/v1",
        "-----BEGIN PGP MESSAGE-----",
        "-----BEGIN AGE ENCRYPTED FILE-----",
        "-----BEGIN ENCRYPTED PRIVATE KEY-----",
        "ENC[AES256_GCM,",
    ];
    if encrypted
        .iter()
        .any(|header| old.contains(header) || new.contains(header))
    {
        return None;
    }
    let old: Vec<_> = old.split_inclusive('\n').collect();
    let new: Vec<_> = new.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_end = old.len() - suffix;
    let new_end = new.len() - suffix;

    let mut patch = format!("\n  {}\n", path_label(path));
    let mut append = |indicator: char, lines: &[&str]| {
        for line in lines {
            let _ = write!(patch, "    {indicator}{line}");
            if !line.ends_with('\n') {
                patch.push_str("\n    \\ No newline at end of file\n");
            }
        }
    };
    append(' ', &old[prefix.saturating_sub(3)..prefix]);
    append('-', &old[prefix..old_end]);
    append('+', &new[prefix..new_end]);
    append(' ', &old[old_end..old_end + suffix.min(3)]);
    Some(patch)
}

fn relative(path: &Path) -> bool {
    path.as_os_str()
        .as_encoded_bytes()
        .split(|&b| b == b'/')
        .all(|part| !part.is_empty() && part != b"." && part != b".." && !part.contains(&0))
}

fn private_path(path: &Path) -> bool {
    let first = path
        .iter()
        .next()
        .and_then(|part| part.to_str())
        .unwrap_or_default();
    matches!(
        first,
        ".git"
            | ".jj"
            | "config.local.toml"
            | "miserc.toml"
            | "mise.lock"
            | "locks"
            | ".setup-state"
    ) || (first.starts_with("config.host-") && first.ends_with(".toml"))
        || (first.starts_with("mise.") && first.ends_with(".lock"))
}

fn encrypted_name(path: &Path) -> bool {
    let path = path.as_os_str().to_string_lossy().to_ascii_lowercase();
    [".age", ".enc", ".gpg", ".pgp", ".sops"]
        .iter()
        .any(|suffix| path.ends_with(suffix))
}

fn text(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.chars()
        .all(|c| c == '\n' || c == '\t' || !terminal_control(c))
        .then_some(text)
}

fn commit(oid: &str) -> Option<String> {
    (matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("{oid}^{{commit}}"))
}

fn blob(tools: &Tools, root: &Path, commit: &str, path: &Path, budget: &Budget) -> Option<Vec<u8>> {
    let entry = run(
        git(tools, root)
            .args(["ls-tree", "-z", commit, "--"])
            .arg(path),
        budget,
    )?;
    if entry.is_empty() {
        return Some(Vec::new());
    }
    let separator = entry.iter().position(|&byte| byte == b'\t')?;
    let name = entry.get(separator + 1..)?.strip_suffix(&[0])?;
    if name != path.as_os_str().as_encoded_bytes() {
        return None;
    }
    let mut fields = std::str::from_utf8(&entry[..separator]).ok()?.split(' ');
    if !matches!(fields.next(), Some("100644" | "100755")) || fields.next()? != "blob" {
        return None;
    }
    let oid = fields.next()?;
    run(git(tools, root).args(["cat-file", "blob", oid]), budget)
}

fn git(tools: &Tools, root: &Path) -> Command {
    let mut command = tools.command("git");
    command
        .current_dir(root)
        .args([
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
        ])
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1");
    command
}

struct Scanner<'a> {
    tools: &'a Tools,
    gitleaks: &'a Path,
    scratch: &'a Path,
    floor: PathBuf,
    project: Option<PathBuf>,
}

impl<'a> Scanner<'a> {
    fn new(
        tools: &'a Tools,
        gitleaks: &'a Path,
        root: &Path,
        scratch: &'a Path,
        budget: &Budget,
    ) -> Option<Self> {
        let floor = scratch.join("floor.toml");
        private_file(&floor, b"[extend]\nuseDefault = true\n").ok()?;
        let project_file = root.join(".gitleaks.toml");
        let project = match fs::symlink_metadata(&project_file) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Ok(_) => Some(snapshot(root, scratch, &project_file, 0, budget)?),
            Err(_) => return None,
        };
        Some(Self {
            tools,
            gitleaks,
            scratch,
            floor,
            project,
        })
    }

    fn clean(&self, path: &Path, patch: &Patch, budget: &Budget) -> bool {
        if budget.expired() {
            return false;
        }
        thread::scope(|scope| {
            let before = scope.spawn(|| self.scan(path, &patch.before, false, budget));
            let after = scope.spawn(|| self.scan(path, &patch.after, false, budget));
            let displayed = self.scan(path, patch.text.as_bytes(), true, budget);
            let before = before.join().unwrap_or(false);
            let after = after.join().unwrap_or(false);
            before && after && displayed
        })
    }

    fn scan(&self, path: &Path, body: &[u8], displayed: bool, budget: &Budget) -> bool {
        let screen = || -> Option<()> {
            if budget.expired() {
                return None;
            }
            let input = private_directory().tempdir_in(self.scratch).ok()?;
            let mut parent = input.path().to_owned();
            for component in path.parent()?.components() {
                parent.push(component);
                if budget.expired() {
                    return None;
                }
                fs::DirBuilder::new().mode(0o700).create(&parent).ok()?;
            }
            let file = input.path().join(path);
            if budget.expired() {
                return None;
            }
            private_file(&file, body).ok()?;
            for config in std::iter::once(&self.floor).chain(&self.project) {
                let stdin = displayed && config == &self.floor;
                let mut command = self.tools.command(self.gitleaks);
                command
                    .current_dir(input.path())
                    .arg(if stdin { "stdin" } else { "dir" })
                    .args([
                        "--no-banner",
                        "--redact",
                        "--ignore-gitleaks-allow",
                        "--report-format",
                        "json",
                        "--report-path",
                        "-",
                        "--gitleaks-ignore-path",
                    ])
                    .arg(self.scratch)
                    .arg("--config")
                    .arg(config);
                if stdin {
                    command.stdin(fs::File::open(&file).ok()?);
                } else {
                    command.arg("--").arg(Path::new(".").join(path));
                }
                let result = run(&mut command, budget)?;
                let report: Vec<serde_json::Value> = serde_json::from_slice(&result).ok()?;
                if !report.is_empty() {
                    return None;
                }
            }
            Some(())
        };
        screen().is_some()
    }
}

// Gitleaks resolves each extension against its working directory. Snapshot only
// checkout-local regular files and rewrite extensions to the private snapshots.
fn snapshot(
    root: &Path,
    scratch: &Path,
    path: &Path,
    depth: usize,
    budget: &Budget,
) -> Option<PathBuf> {
    if budget.expired() || depth > 2 || fs::canonicalize(path).ok()?.as_path() != path {
        return None;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut config = String::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_string(&mut config)
        .ok()?;
    if config.len() > MAX_BYTES {
        return None;
    }
    let mut doc = config.parse::<DocumentMut>().ok()?;
    if doc.iter().any(|(key, _)| {
        key.contains('.') || (key.eq_ignore_ascii_case("extend") && key != "extend")
    }) {
        return None;
    }
    if let Some(extend) = doc.get_mut("extend") {
        let extend = extend.as_table_like_mut()?;
        if extend
            .iter()
            .any(|(key, _)| !matches!(key, "path" | "useDefault" | "disabledRules"))
        {
            return None;
        }
        if let Some(item) = extend.get_mut("path") {
            let extension = item.as_str()?;
            if !extension.is_empty() {
                let extension = Path::new(extension);
                if !relative(extension) || private_path(extension) {
                    return None;
                }
                let copy = snapshot(root, scratch, &root.join(extension), depth + 1, budget)?;
                *item = value(copy.to_str()?);
            }
        }
    }
    let copy = scratch.join(format!("project-{depth}.toml"));
    private_file(&copy, doc.to_string().as_bytes()).ok()?;
    Some(copy)
}

fn run(command: &mut Command, budget: &Budget) -> Option<Vec<u8>> {
    child::run(command, budget, MAX_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_extension_path_keeps_the_default_floor() {
        let checkout = private_directory().tempdir().unwrap();
        let root = checkout.path().canonicalize().unwrap();
        let config = root.join(".gitleaks.toml");
        let local_rules = "title = 'Local fixture rules'\n";
        fs::write(root.join("rules.toml"), local_rules).unwrap();
        let tools = Tools(Vec::new());
        let budget = Budget::new(Instant::now() + Duration::from_secs(15));

        for (name, settings, extends_local_file) in [
            ("missing", "useDefault = true\n", false),
            ("nonempty", "path = 'rules.toml'\n", true),
            ("empty", "useDefault = true\npath = ''\n", false),
        ] {
            fs::write(&config, format!("[extend]\n{settings}")).unwrap();
            let scratch = private_directory().tempdir().unwrap();
            let scanner = Scanner::new(
                &tools,
                Path::new("gitleaks"),
                &root,
                scratch.path(),
                &budget,
            )
            .unwrap_or_else(|| panic!("{name} extension path rejected"));
            assert_eq!(
                fs::read_to_string(scanner.floor).unwrap(),
                "[extend]\nuseDefault = true\n"
            );
            let project = fs::read_to_string(scanner.project.unwrap()).unwrap();
            let local_snapshot = scratch.path().join("project-1.toml");
            if extends_local_file {
                let doc: DocumentMut = project.parse().unwrap();
                assert_eq!(doc["extend"]["path"].as_str(), local_snapshot.to_str());
                assert_eq!(fs::read_to_string(local_snapshot).unwrap(), local_rules);
            } else {
                assert_eq!(project, format!("[extend]\n{settings}"));
                assert!(!local_snapshot.exists());
            }
        }
    }

    #[test]
    fn cutoff_returns_screened_text_without_racing_a_staging_write() {
        let (started, staged) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (finish, finished) = mpsc::channel();
        let screened = collect_until(
            Instant::now() + Duration::from_secs(2),
            move |budget, send| {
                let directory = private_directory().tempdir().unwrap();
                let path = directory.path().join("input");
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .unwrap();
                send.send("already screened\n".into()).unwrap();
                started.send(path).unwrap();
                // Keep an in-flight staging operation blocked across the cutoff.
                blocked.recv().unwrap();
                file.write_all(b"staging completed").unwrap();
                finish.send(budget.expired()).unwrap();
            },
        );

        let staged = staged.try_recv();
        let scratch_owned_by_worker = staged.as_ref().is_ok_and(|path| path.is_file());
        let already_screened = screened.text == "already screened\n";
        let _ = release.send(());
        drop(screened);

        assert!(scratch_owned_by_worker);
        assert!(already_screened);
        assert!(finished.try_recv().unwrap());
        assert!(!staged.unwrap().exists());
    }
}
