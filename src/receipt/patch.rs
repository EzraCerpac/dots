use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{fs, thread};

use nix::libc;
use tempfile::TempDir;
use toml_edit::{DocumentMut, value};

use super::{ChangeGroup, Receipt, local_command, path_label, terminal_control};
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
) -> String {
    render_until(
        env,
        root,
        receipt,
        replay,
        bytes,
        Instant::now() + SCREEN_BUDGET,
    )
}

fn render_until(
    env: &Env,
    root: &Path,
    receipt: &Receipt,
    replay: bool,
    bytes: usize,
    deadline: Instant,
) -> String {
    let candidates: Vec<_> = receipt
        .groups()
        .into_iter()
        .flat_map(|(_, groups)| groups.iter())
        .flat_map(|group| group.paths.iter().map(move |path| (group, path)))
        .collect();
    if candidates.is_empty() || bytes < OMISSION_BYTES || (!replay && candidates.len() > 2) {
        return String::new();
    }

    let mut output = String::new();
    let mut omitted = candidates.len();
    if receipt.checkout.as_deref() == Some(root)
        && run(
            git(env, root).args(["rev-parse", "--show-prefix"]),
            deadline,
        )
        .as_deref()
            == Some(b"\n")
        && let Some(scanner) = Scanner::new(env, root)
    {
        let mut lines = 0;
        for (group, path) in candidates {
            if Instant::now() >= deadline {
                break;
            }
            let Some(patch) = prepare(env, root, group, path, deadline) else {
                continue;
            };
            let patch_lines = patch.text.lines().count();
            let extra = if replay { OMISSION_BYTES } else { 0 };
            if output.len() + patch.text.len() + extra > bytes
                || (!replay && lines + patch_lines > 12)
            {
                continue;
            }
            if [&patch.before[..], &patch.after, patch.text.as_bytes()]
                .into_iter()
                .all(|body| scanner.clean(path, body, deadline))
            {
                lines += patch_lines;
                output.push_str(&patch.text);
                omitted -= 1;
            }
        }
    }
    if replay && omitted != 0 {
        let files = if omitted == 1 { "file" } else { "files" };
        let _ = writeln!(output, "\nPatch omitted for {omitted} {files}.");
    }
    output
}

struct Patch {
    before: Vec<u8>,
    after: Vec<u8>,
    text: String,
}

fn prepare(
    env: &Env,
    root: &Path,
    group: &ChangeGroup,
    path: &Path,
    deadline: Instant,
) -> Option<Patch> {
    if !relative(path) || encrypted_name(path) {
        return None;
    }
    let before = commit(group.before.as_deref()?)?;
    let after = commit(group.after.as_deref()?)?;
    let old = blob(env, root, &before, path, deadline)?;
    let new = blob(env, root, &after, path, deadline)?;
    if old == new
        || [&old, &new].into_iter().any(|body| {
            text(body).is_none_or(|text| {
                text.contains("age-encryption.org/v1")
                    || text.contains("-----BEGIN PGP MESSAGE-----")
                    || text.contains("-----BEGIN AGE ENCRYPTED FILE-----")
                    || text.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----")
                    || text.contains("ENC[AES256_GCM,")
            })
        })
    {
        return None;
    }

    let raw = run(
        git(env, root)
            .args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--no-renames",
                "--no-relative",
                "--unified=3",
                &before,
                &after,
                "--",
            ])
            .arg(path),
        deadline,
    )?;
    let raw = text(&raw)?;
    let mut hunks = raw.lines().skip_while(|line| !line.starts_with("@@ "));
    let first = hunks.next()?;
    let mut patch = format!("\n  {}\n", path_label(path));
    let multi_hunk = hunks.clone().any(|line| line.starts_with("@@ "));
    if multi_hunk {
        let _ = writeln!(patch, "    {first}");
    }
    for line in hunks {
        let _ = writeln!(patch, "    {line}");
    }
    Some(Patch {
        before: old,
        after: new,
        text: patch,
    })
}

fn relative(path: &Path) -> bool {
    path.as_os_str()
        .as_encoded_bytes()
        .split(|&b| b == b'/')
        .all(|part| !part.is_empty() && part != b"." && part != b".." && !part.contains(&0))
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

fn blob(env: &Env, root: &Path, commit: &str, path: &Path, deadline: Instant) -> Option<Vec<u8>> {
    let entry = run(
        git(env, root)
            .args(["ls-tree", "-z", commit, "--"])
            .arg(path),
        deadline,
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
    run(git(env, root).args(["cat-file", "blob", oid]), deadline)
}

fn git(env: &Env, root: &Path) -> Command {
    let mut command = local_command(env, "git");
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
    env: &'a Env,
    scratch: TempDir,
    floor: PathBuf,
    project: Option<PathBuf>,
}

impl<'a> Scanner<'a> {
    fn new(env: &'a Env, root: &Path) -> Option<Self> {
        let scratch = tempfile::tempdir().ok()?;
        let floor = scratch.path().join("floor.toml");
        fs::write(&floor, "[extend]\nuseDefault = true\n").ok()?;
        let project_file = root.join(".gitleaks.toml");
        let project = match fs::symlink_metadata(&project_file) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Ok(_) => Some(snapshot(root, scratch.path(), &project_file, 0)?),
            Err(_) => return None,
        };
        Some(Self {
            env,
            scratch,
            floor,
            project,
        })
    }

    fn clean(&self, path: &Path, body: &[u8], deadline: Instant) -> bool {
        if Instant::now() >= deadline {
            return false;
        }
        let screen = || -> Option<()> {
            // The disposable directory keeps the original path relevant to path-scoped rules.
            let input = tempfile::tempdir_in(self.scratch.path()).ok()?;
            let file = input.path().join(path);
            fs::create_dir_all(file.parent()?).ok()?;
            let mut private = fs::OpenOptions::new();
            use std::os::unix::fs::OpenOptionsExt as _;
            private
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(file)
                .ok()?
                .write_all(body)
                .ok()?;
            for config in std::iter::once(&self.floor).chain(&self.project) {
                let result = run(
                    local_command(self.env, "gitleaks")
                        .current_dir(input.path())
                        .args([
                            "dir",
                            "--no-banner",
                            "--redact",
                            "--ignore-gitleaks-allow",
                            "--report-format",
                            "json",
                            "--report-path",
                            "-",
                            "--gitleaks-ignore-path",
                        ])
                        .arg(self.scratch.path())
                        .arg("--config")
                        .arg(config)
                        .arg("--")
                        .arg(Path::new(".").join(path)),
                    deadline,
                )?;
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
fn snapshot(root: &Path, scratch: &Path, path: &Path, depth: usize) -> Option<PathBuf> {
    use std::os::unix::fs::OpenOptionsExt as _;

    if depth > 2 || fs::canonicalize(path).ok()?.as_path() != path {
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
            let extension = Path::new(item.as_str()?);
            if !relative(extension) {
                return None;
            }
            let copy = snapshot(root, scratch, &root.join(extension), depth + 1)?;
            *item = value(copy.to_str()?);
        }
    }
    let copy = scratch.join(format!("project-{depth}.toml"));
    fs::write(&copy, doc.to_string()).ok()?;
    Some(copy)
}

// A single deadline covers all Git reads and scans. No child writes to the
// terminal, and a stalled child group cannot delay returning to metadata.
fn run(command: &mut Command, deadline: Instant) -> Option<Vec<u8>> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes);
        let _ = send.send(
            read.ok()
                .filter(|_| bytes.len() <= MAX_BYTES)
                .map(|_| bytes),
        );
    });
    let mut status = None;
    let mut output = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(_) => break,
            }
        }
        if output.is_none() {
            match receive.try_recv() {
                Ok(Some(bytes)) => output = Some(bytes),
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let (Some(status), Some(_)) = (status, &output) {
            return if status.success() && Instant::now() < deadline {
                output
            } else {
                None
            };
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(4)));
    }
    // SAFETY: process_group(0) isolated this child and its descendants.
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    let _ = child.wait();
    None
}
