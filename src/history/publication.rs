//! The checkpoints a push adds to the destination host branch are the unit of
//! inspection. Current enrollment cannot account for a path removed from it.

use std::collections::{HashMap, HashSet};
use std::process::{Command, Output};
use std::str;

use serde::Deserialize;

use crate::Fail;

const LOCAL: &str = "refs/heads/main";
const MARKER: &str = ".mise-history/format.toml";
const MANIFEST: &str = ".mise-history/manifest.json";
const MAGIC: &[u8] = b"mise-encrypted-file-v1\n";
const REVIEW: &str = "Keep the current store and inspect the checkpoint in a private copy. To publish fresh history, copy your configuration with encrypted tracking, set its history origin and dots_state_repo to an empty private repository, and run dots backup with a separate MISE_STATE_DIR";
const RECONCILE: &str = "Inspect and reconcile the destination host branch in a separate private clone, then retry backup";

pub(super) fn allows_plaintext(output: Output) -> Result<bool, Fail> {
    match (output.status.success(), line(&output.stdout)) {
        (true, b"true") => Ok(true),
        (true, b"false") => Ok(false),
        _ => Err(refusal(
            "could not read mise's effective history.allow_plaintext_history setting",
            "Check the setting with `mise settings get history.allow_plaintext_history` and retry backup",
        )),
    }
}

/// Read-only preflight for native sync. The launcher is bound to the native
/// bare store and the origin and branch have already passed backup's checks.
pub(super) fn preflight(
    git: impl Fn() -> Result<Command, Fail>,
    probe: impl Fn(Command) -> Result<Output, Fail>,
    branch: &str,
    origin: &str,
) -> Result<(), Fail> {
    let blocked = || refusal("could not verify saved settings history", RECONCILE);
    let probe = |args: &[&str]| {
        let mut command = git()?;
        command
            .env("LC_ALL", "C")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_GRAFT_FILE", "/dev/null")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(args);
        probe(command).map_err(|_| blocked())
    };
    let read = |args: &[&str]| {
        let output = probe(args)?;
        output
            .status
            .success()
            .then_some(output.stdout)
            .ok_or_else(blocked)
    };
    if read(&["rev-parse", "--is-shallow-repository"])? != b"false\n" {
        return Err(blocked());
    }
    let local_tip = || -> Result<Option<String>, Fail> {
        let output = read(&["for-each-ref", "--format=%(objectname) %(refname)", LOCAL])?;
        let Some(id) = line(&output).strip_suffix(b" refs/heads/main") else {
            return if output.is_empty() {
                Ok(None)
            } else {
                Err(blocked())
            };
        };
        parse_oid(id)
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(blocked)
    };
    let dest = format!("refs/heads/{branch}");
    let remote_tip = || -> Result<Option<String>, Fail> {
        let output = probe(&["ls-remote", "--refs", "--exit-code", "--", origin, &dest])?;
        match output.status.code() {
            Some(2) if output.stdout.is_empty() => Ok(None),
            Some(0) => {
                let (id, name) = split_byte(line(&output.stdout), b'\t').ok_or_else(blocked)?;
                if name != dest.as_bytes() {
                    return Err(blocked());
                }
                parse_oid(id)
                    .map(str::to_owned)
                    .map(Some)
                    .ok_or_else(blocked)
            }
            _ => Err(blocked()),
        }
    };
    let local = local_tip()?;
    if let Some(id) = local.as_deref()
        && read(&["cat-file", "-t", id]).ok().as_deref() != Some(b"commit\n")
    {
        return Err(refusal(
            format!("cannot verify history path \"<checkpoint>\" in checkpoint {id}"),
            REVIEW,
        ));
    }
    let remote = remote_tip()?;
    if let Some(local) = local.as_deref()
        && Some(local) != remote.as_deref()
    {
        // If the remote commit is unfamiliar, avoid fetching into the store.
        // Auditing all local ancestors is a safe superset in that case.
        let known_remote = if let Some(remote) = remote.as_deref() {
            let object = probe(&["cat-file", "-t", remote])?;
            if object.status.success() {
                if object.stdout != b"commit\n" {
                    return Err(blocked());
                }
                Some(remote)
            } else {
                None
            }
        } else {
            None
        };
        // Native v1 reconciles to a push only if the resulting tree matches
        // the local saved tree. A merge adds the remote parent and that same
        // tree. Neither makes any unchecked local ancestor reachable.
        audit(&read, local, known_remote)?;
    }
    if remote_tip()? != remote || local_tip()? != local {
        return Err(refusal(
            "the local or destination checkpoint changed during verification",
            "Retry backup",
        ));
    }
    Ok(())
}

fn refusal(diagnostic: impl std::fmt::Display, next: &str) -> Fail {
    Fail::new(1, format!("{diagnostic}. {next}."))
}

fn split_byte(output: &[u8], byte: u8) -> Option<(&[u8], &[u8])> {
    let at = output.iter().position(|&b| b == byte)?;
    Some((&output[..at], &output[at + 1..]))
}

fn line(output: &[u8]) -> &[u8] {
    output.strip_suffix(b"\n").unwrap_or(output)
}

fn parse_oid(id: &[u8]) -> Option<&str> {
    (matches!(id.len(), 40 | 64)
        && id
            .iter()
            .all(|&b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && id.iter().any(|&b| b != b'0'))
    .then(|| str::from_utf8(id).ok())
    .flatten()
}

fn audit(
    read: &impl Fn(&[&str]) -> Result<Vec<u8>, Fail>,
    local: &str,
    remote: Option<&str>,
) -> Result<(), Fail> {
    let mut args = vec!["rev-list", "--reverse", "--topo-order", local];
    let exclude = remote.map(|id| format!("^{id}"));
    args.extend(exclude.as_deref());
    args.push("--");
    let commits = read(&args)?;
    if commits.is_empty() {
        return Ok(());
    }
    let mut manifests = HashMap::new();
    let mut markers = HashSet::new();
    let mut checked = HashSet::new();
    for id in line(&commits).split(|&b| b == b'\n') {
        let id = parse_oid(id)
            .ok_or_else(|| refusal("could not enumerate the checkpoints to publish", REVIEW))?;
        let unverified = |path: &str| {
            refusal(
                format!("cannot verify history path {path:?} in checkpoint {id}"),
                REVIEW,
            )
        };
        let tree = read(&["ls-tree", "-rz", "--full-tree", id, "--"])
            .map_err(|_| unverified("<checkpoint tree>"))?;
        let tree = tree
            .strip_suffix(b"\0")
            .ok_or_else(|| unverified("<checkpoint tree>"))?;
        let entries: Vec<Entry<'_>> = tree
            .split(|&b| b == 0)
            .map(|record| {
                Entry::parse(record).ok_or_else(|| {
                    let path =
                        split_byte(record, b'\t').and_then(|(_, path)| str::from_utf8(path).ok());
                    unverified(path.unwrap_or("<invalid tree entry>"))
                })
            })
            .collect::<Result<_, _>>()?;
        let unique: HashSet<_> = entries.iter().map(|entry| entry.path).collect();
        if unique.len() != entries.len() {
            return Err(unverified("<duplicate tree entry>"));
        }
        let metadata = |path| {
            entries
                .iter()
                .find(|entry| entry.path == path && entry.mode == "100644")
                .ok_or_else(|| unverified(path))
        };
        let marker = metadata(MARKER)?;
        if markers.insert(marker.oid.to_owned()) {
            let body = blob(read, marker.oid, 65536).ok_or_else(|| unverified(MARKER))?;
            let format = str::from_utf8(&body)
                .ok()
                .and_then(|input| input.parse::<toml_edit::DocumentMut>().ok())
                .and_then(|doc| doc.get("format")?.as_integer());
            if format != Some(1) {
                return Err(unverified(MARKER));
            }
        }
        let manifest = metadata(MANIFEST)?;
        if !manifests.contains_key(manifest.oid) {
            let body =
                blob(read, manifest.oid, 4 * 1024 * 1024).ok_or_else(|| unverified(MANIFEST))?;
            let decoded: Manifest =
                serde_json::from_slice(&body).map_err(|_| unverified(MANIFEST))?;
            if decoded.format != 1
                || decoded.enrollment.iter().any(|entry| {
                    !safe_path(&entry.path)
                        || !matches!(entry.path.split('/').next(), Some("config" | "home"))
                })
            {
                return Err(unverified(MANIFEST));
            }
            manifests.insert(manifest.oid.to_owned(), decoded);
        }
        let manifest = &manifests[manifest.oid];
        for entry in entries {
            let path = entry.path;
            let (root, _) = path.split_once('/').unwrap_or((path, ""));
            let (root, variant) = root
                .split_once('@')
                .map_or((root, None), |(root, variant)| (root, Some(variant)));
            match (root, variant) {
                _ if matches!(path, MARKER | MANIFEST) => continue,
                ("config", None) if !manifest.tracks_config(path) => continue,
                ("home" | "config", Some(variant))
                    if !variant.is_empty() && !variant.contains('@') => {}
                ("home", None) => {}
                ("config", None) => {}
                _ => return Err(unverified(path)),
            }
            if !checked.insert((path.to_owned(), entry.mode.to_owned(), entry.oid.to_owned())) {
                continue;
            }
            let data = blob(read, entry.oid, 256 * 1024 * 1024).ok_or_else(|| unverified(path))?;
            if entry.mode != "100644" || !encrypted_file(&data, path) {
                return Err(refusal(
                    format!(
                        "plaintext or invalid encrypted payload at {path:?} in checkpoint {id}"
                    ),
                    REVIEW,
                ));
            }
        }
    }
    Ok(())
}

fn blob(
    read: &impl Fn(&[&str]) -> Result<Vec<u8>, Fail>,
    oid: &str,
    limit: usize,
) -> Option<Vec<u8>> {
    if read(&["cat-file", "-t", oid]).ok()? != b"blob\n" {
        return None;
    }
    let size = read(&["cat-file", "-s", oid]).ok()?;
    let size: usize = str::from_utf8(line(&size)).ok()?.parse().ok()?;
    if size > limit {
        return None;
    }
    let data = read(&["cat-file", "blob", oid]).ok()?;
    (data.len() == size).then_some(data)
}

struct Entry<'a> {
    mode: &'a str,
    oid: &'a str,
    path: &'a str,
}

impl<'a> Entry<'a> {
    fn parse(record: &'a [u8]) -> Option<Self> {
        let (header, path) = split_byte(record, b'\t')?;
        let mut fields = str::from_utf8(header).ok()?.split(' ');
        let mode = fields.next()?;
        let kind = fields.next()?;
        let oid = parse_oid(fields.next()?.as_bytes())?;
        let path = str::from_utf8(path).ok()?;
        (fields.next().is_none()
            && safe_path(path)
            && matches!((kind, mode), ("blob", "100644" | "100755" | "120000")))
        .then_some(Self { mode, oid, path })
    }
}

fn safe_path(path: &str) -> bool {
    path.split('/').all(|part| {
        !matches!(part, "" | "." | "..")
            && !part.eq_ignore_ascii_case(".git")
            && !part.contains(['\\', ':'])
            && !part.chars().any(char::is_control)
    })
}

#[derive(Deserialize)]
struct Manifest {
    format: u32,
    enrollment: Vec<Enrollment>,
    #[serde(rename = "exclude")]
    _exclude: Vec<String>,
    #[serde(rename = "recipients")]
    _recipients: Vec<String>,
    #[serde(rename = "permissions")]
    _permissions: HashMap<String, u32>,
}

#[derive(Deserialize)]
struct Enrollment {
    path: String,
    #[serde(rename = "autosave")]
    _autosave: bool,
    #[serde(rename = "encrypt")]
    _encrypt: bool,
    variants: Vec<serde_json::Map<String, serde_json::Value>>,
}

impl Manifest {
    fn tracks_config(&self, path: &str) -> bool {
        self.enrollment.iter().any(|entry| {
            entry.variants.is_empty()
                && (path == entry.path
                    || path
                        .strip_prefix(&entry.path)
                        .is_some_and(|rest| rest.starts_with('/')))
        })
    }
}

// Mise v1 writes a named four-field MessagePack map with a binary age file.
// Validate its exact shape and path; a magic prefix alone proves nothing.
fn encrypted_file(data: &[u8], path: &str) -> bool {
    let Some(mut fields) = data
        .strip_prefix(MAGIC)
        .and_then(|data| data.strip_prefix(&[0x84]))
    else {
        return false;
    };
    let mut valid = 0;
    for _ in 0..4 {
        let Some(key) = messagepack(&mut fields, false) else {
            return false;
        };
        let Some(value) = messagepack(&mut fields, key == b"ciphertext") else {
            return false;
        };
        let field = match key {
            b"path" if value == path.as_bytes() => 1,
            b"mode" if matches!(value, b"100644" | b"100755" | b"120000") => 2,
            b"scheme" if value.len() == 64 && value.iter().all(u8::is_ascii_hexdigit) => 4,
            b"ciphertext" if age_file(value) => 8,
            _ => return false,
        };
        valid |= field;
    }
    valid == 15 && fields.is_empty()
}

fn messagepack<'a>(input: &mut &'a [u8], binary: bool) -> Option<&'a [u8]> {
    let (&tag, rest) = input.split_first()?;
    let (size, rest) = match (binary, tag) {
        (false, 0xa0..=0xbf) => (usize::from(tag & 31), rest),
        (false, 0xd9) | (true, 0xc4) => (usize::from(*rest.first()?), rest.get(1..)?),
        (false, 0xda) | (true, 0xc5) => (
            u16::from_be_bytes(rest.get(..2)?.try_into().ok()?) as usize,
            rest.get(2..)?,
        ),
        (false, 0xdb) | (true, 0xc6) => (
            u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize,
            rest.get(4..)?,
        ),
        _ => return None,
    };
    let (value, remainder) = rest.split_at_checked(size)?;
    *input = remainder;
    Some(value)
}

// Structural validation only. Without the recipient keys we cannot
// authenticate the header, verify the payload or prove who can decrypt it.
fn age_file(bytes: &[u8]) -> bool {
    let Some(mut bytes) = bytes.strip_prefix(b"age-encryption.org/v1\n") else {
        return false;
    };
    let base64 = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/');
    let mut stanza = false;
    while let Some((line, rest)) = split_byte(bytes, b'\n') {
        if let Some(mac) = line.strip_prefix(b"--- ") {
            return stanza
                && mac.len() == 43
                && mac.iter().copied().all(base64)
                && rest.len() >= 32;
        }
        if let Some(args) = line.strip_prefix(b"-> ") {
            if args.is_empty() || !args.iter().all(|b| (b' '..=b'~').contains(b)) {
                return false;
            }
            stanza = true;
        } else if !stanza || line.len() > 64 || !line.iter().copied().all(base64) {
            return false;
        }
        bytes = rest;
    }
    false
}
