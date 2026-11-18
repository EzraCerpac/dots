use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

use super::{
    Request,
    bundle::{self, Bundle},
    env_string, source,
};
use crate::config::{Doc, RepoSetting, Vars};
use crate::run::{Issue, Outcome, Run, StageId};
use crate::{Ctx, Fail};

const BUNDLE_IDENTITY: &[&str] = &["vars", "bootstrap_bundle_identity"];
const RECOVERY_IDENTITY: &[&str] = &["env", "SETUP_AGE_IDENTITY"];
const HISTORY_ORIGIN: &[&str] = &["env", "SETUP_HISTORY_ORIGIN"];
const HISTORY_URL: &[&str] = &["history", "origin", "url"];
const HISTORY_BRANCH: &[&str] = &["history", "origin", "branch"];
const AGE_FILES: &[&str] = &["settings", "age", "identity_files"];

pub(super) fn local(ctx: &Ctx) -> Result<Doc, Fail> {
    Doc::open(&ctx.root.path().join("config.local.toml"))
}

fn find<'a>(doc: &'a DocumentMut, path: &[&str]) -> Option<&'a Item> {
    let mut item = doc.as_item();
    for key in path {
        item = item.as_table_like()?.get(key)?;
    }
    Some(item)
}

fn string<'a>(doc: &'a DocumentMut, path: &[&str]) -> Result<Option<&'a str>, Fail> {
    find(doc, path)
        .map(|item| {
            item.as_str().ok_or_else(|| {
                Fail::usage(format!(
                    "config.local.toml {} must be a string",
                    path.join(".")
                ))
            })
        })
        .transpose()
}

fn set(doc: &mut DocumentMut, path: &[&str], mut value: Value) -> Result<(), Fail> {
    let (key, parents) = path.split_last().expect("nonempty key path");
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for (index, parent) in parents.iter().enumerate() {
        if !table.contains_key(parent) {
            let mut child = Table::new();
            child.set_implicit(index + 1 < parents.len());
            table.insert(parent, Item::Table(child));
        }
        table = table
            .get_mut(parent)
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| {
                Fail::usage(format!(
                    "config.local.toml {} must be a table",
                    parents[..=index].join(".")
                ))
            })?;
    }
    if let Some(previous) = table.get(key).and_then(Item::as_value) {
        *value.decor_mut() = previous.decor().clone();
    }
    table.insert(key, Item::Value(value));
    Ok(())
}

fn identity_path(ctx: &Ctx, path: &OsStr) -> Result<PathBuf, Fail> {
    let path = Path::new(path);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    if let Ok(after_home) = path.strip_prefix("~")
        && let Some(home) = ctx.env.home()
    {
        return Ok(home.join(after_home));
    }
    Err(Fail::usage(
        "an age identity must be an absolute path or start with ~/",
    ))
}

fn readable_identity(ctx: &Ctx, path: &Path) -> Result<Option<PathBuf>, Fail> {
    let physical = match fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Fail::new(
                1,
                format!("cannot resolve the age identity: {error}"),
            ));
        }
    };
    if physical.starts_with(ctx.root.path()) {
        return Err(Fail::usage(
            "the age identity must be outside the configuration root",
        ));
    }
    if !fs::metadata(&physical).is_ok_and(|metadata| metadata.is_file())
        || File::open(&physical).is_err()
    {
        return Err(Fail::new(
            1,
            "the age identity must be a readable regular file",
        ));
    }
    Ok(Some(physical))
}

pub(super) fn open_bundle(
    ctx: &Ctx,
    run: &Run<'_>,
    stage: StageId,
    request: &Request,
    profile: &str,
) -> Result<(Outcome, Option<Bundle>), Fail> {
    let mut local = local(ctx)?;
    let remembered = string(&local.document, BUNDLE_IDENTITY)?;
    let default = ctx
        .root
        .path()
        .join(format!("encrypted/bootstrap.{profile}.json.age"));
    let file = request.bundle.as_deref().unwrap_or(&default);
    let exists = file
        .try_exists()
        .map_err(|error| Fail::new(1, format!("cannot locate the bootstrap bundle: {error}")))?;
    if !exists && request.bundle.is_none() && request.identity.is_none() && remembered.is_none() {
        return Ok((Outcome::Completed, None));
    }
    let identity = if let Some(identity) = &request.identity {
        identity.clone()
    } else if let Some(identity) = remembered {
        identity_path(ctx, identity.as_ref())?
    } else {
        recovery_identity(ctx, &local.document, request)?
    };
    if exists && let Some(identity) = readable_identity(ctx, &identity)? {
        let bundle = bundle::decrypt(ctx, run, file, &identity)?;
        if remembered.is_none_or(|value| Path::new(value) != identity) {
            let identity = identity.to_str().ok_or_else(|| {
                Fail::usage(
                    "the bundle identity path must be UTF-8 to save it in config.local.toml",
                )
            })?;
            set(&mut local.document, BUNDLE_IDENTITY, Value::from(identity))?;
            local.make_private();
            local.install()?;
        }
        return Ok((Outcome::Completed, Some(bundle)));
    }
    run.issue(
        stage,
        Issue::new(
            "Supply the encrypted bundle and an external age identity.",
            "dots bootstrap --bundle FILE --identity FILE",
        ),
    );
    Ok((Outcome::Deferred, None))
}

fn recovery_identity(ctx: &Ctx, local: &DocumentMut, request: &Request) -> Result<PathBuf, Fail> {
    if let Some(identity) = &request.identity {
        Ok(identity.clone())
    } else if let Some(identity) = ctx
        .env
        .get("SETUP_AGE_IDENTITY")
        .filter(|value| !value.is_empty())
    {
        identity_path(ctx, identity)
    } else if let Some(identity) = string(local, RECOVERY_IDENTITY)? {
        identity_path(ctx, identity.as_ref())
    } else {
        ctx.env
            .home()
            .map(|home| home.join(".config/age/keys.txt"))
            .ok_or_else(|| Fail::usage("HOME is needed to locate the default age identity"))
    }
}

pub(super) fn history_requested(ctx: &Ctx, vars: &Vars) -> Result<bool, Fail> {
    if vars.state_repo.is_some() || env_string(ctx, "SETUP_HISTORY_ORIGIN")?.is_some() {
        return Ok(true);
    }
    let local = local(ctx)?;
    Ok(string(&local.document, HISTORY_ORIGIN)?.is_some()
        || string(&local.document, HISTORY_URL)?.is_some())
}

pub(super) fn persistence_origin(ctx: &Ctx) -> Result<Option<String>, Fail> {
    let Some(provided) = env_string(ctx, "SETUP_HISTORY_ORIGIN")? else {
        return Ok(None);
    };
    let doc = local(ctx)?;
    if string(&doc.document, HISTORY_ORIGIN)?.is_some()
        || string(&doc.document, HISTORY_URL)?.is_some()
    {
        // Restore and enrollment check this value before making history changes.
        return Ok(None);
    }
    RepoSetting::parse(provided)?;
    Ok(Some(source::transport_url(provided)))
}

pub(super) struct ResolvedHistory {
    pub origin: RepoSetting,
    pub identity: PathBuf,
}

pub(super) fn resolve_history(
    ctx: &Ctx,
    request: &Request,
    vars: &Vars,
    machine: &str,
    restoring: bool,
) -> Result<Option<ResolvedHistory>, Fail> {
    let mut local = local(ctx)?;
    let identity = recovery_identity(ctx, &local.document, request)?;
    let recorded_identity = string(&local.document, RECOVERY_IDENTITY)?
        .map(|value| identity_path(ctx, value.as_ref()))
        .transpose()?;
    if let Some(recorded) = &recorded_identity {
        let physical = |path: &Path| {
            crate::config::physical(path).map_err(|error| {
                Fail::new(
                    1,
                    format!("could not resolve the recovery identity: {error}"),
                )
            })
        };
        if physical(recorded)? != physical(&identity)? {
            return Err(Fail::usage(
                "the recorded recovery identity differs from the selected age identity",
            ));
        }
    }
    let Some(identity) = readable_identity(ctx, &identity)? else {
        return Ok(None);
    };
    let expected = vars.state_repo.as_ref().ok_or_else(|| {
        Fail::usage("config.toml needs [vars] dots_state_repo for settings history")
    })?;
    let recorded_url = string(&local.document, HISTORY_URL)?;
    let recorded_origin = string(&local.document, HISTORY_ORIGIN)?;
    let provided = env_string(ctx, "SETUP_HISTORY_ORIGIN")?;
    for origin in [recorded_url, recorded_origin, provided]
        .into_iter()
        .flatten()
    {
        if !expected.matches(origin) {
            return Err(Fail::usage(
                "the history origin does not match dots_state_repo",
            ));
        }
    }
    let url = match recorded_url.or(recorded_origin).or(provided) {
        Some(value) => source::transport_url(value),
        None => source::configured_transport(ctx, "dots_state_repo")?,
    };
    let origin = RepoSetting::parse(&url)?;
    if restoring {
        let branch = format!("host-{machine}");
        if string(&local.document, HISTORY_BRANCH)?.is_some_and(|old| old != branch) {
            return Err(Fail::usage(
                "the recorded history branch differs from the restore machine",
            ));
        }

        let identity_value = identity.to_str().ok_or_else(|| {
            Fail::usage("the recovery identity path must be UTF-8 to save it in config.local.toml")
        })?;
        let mut identities = match find(&local.document, AGE_FILES) {
            Some(value) => value
                .as_array()
                .cloned()
                .ok_or_else(|| Fail::usage("settings.age.identity_files must be an array"))?,
            None => Array::new(),
        };
        if !identities
            .iter()
            .any(|value| value.as_str() == Some(identity_value))
        {
            identities.push(identity_value);
        }
        for (key, value) in [
            (HISTORY_ORIGIN, url.as_str()),
            (HISTORY_URL, url.as_str()),
            (HISTORY_BRANCH, &branch),
            (RECOVERY_IDENTITY, identity_value),
        ] {
            if find(&local.document, key).is_none() {
                set(&mut local.document, key, Value::from(value))?;
            }
        }
        set(&mut local.document, AGE_FILES, Value::Array(identities))?;
        set(
            &mut local.document,
            &["settings", "history", "sync"],
            Value::from("manual"),
        )?;
        local.make_private();
        local.install()?;
    }
    Ok(Some(ResolvedHistory { origin, identity }))
}

pub(super) fn enroll(
    ctx: &Ctx,
    run: &Run<'_>,
    stage: StageId,
    profile: &str,
    machine: &str,
    resolved: ResolvedHistory,
    bundle_recipients: &[String],
) -> Result<Outcome, Fail> {
    use crate::history::{self, Enroll};
    use crate::process::{Cwd, Mise};
    use crate::run::Policy;

    let mut derive = ctx.command("age-keygen", Cwd::Root)?;
    derive.arg("-y").arg(&resolved.identity);
    let public = run.probe(derive).map_err(age_keygen_error)?;
    let public = String::from_utf8(public.stdout)
        .ok()
        .filter(|_| public.status.success())
        .ok_or_else(|| Fail::new(1, "could not read the recovery identity's public recipient"))?;
    let mut recipients = bundle_recipients.to_vec();
    for key in public.lines() {
        if !super::bundle::recipient(key) {
            return Err(Fail::new(
                1,
                "age-keygen returned an invalid public recipient",
            ));
        }
        if !recipients.iter().any(|entry| entry == key) {
            recipients.push(key.to_owned());
        }
    }
    if public.is_empty() {
        return Err(Fail::new(
            1,
            "the recovery identity has no public recipient",
        ));
    }
    let identity = machine_identity(ctx, run)?;
    let had_recovery = string(&local(ctx)?.document, RECOVERY_IDENTITY)?.is_some();
    let outcome = history::enroll(
        run,
        stage,
        Enroll {
            profile,
            machine,
            origin: &resolved.origin,
            identity: &identity,
            recipients: &recipients,
        },
    );
    if outcome != Outcome::Completed || run.cancelled() {
        return Ok(outcome);
    }

    let mut local = local(ctx)?;
    // History preparation can fill an absent recovery setting with the machine key.
    let recorded = string(&local.document, RECOVERY_IDENTITY)?;
    if recorded.is_none() || (!had_recovery && recorded.map(Path::new) == Some(identity.as_path()))
    {
        let recovery = resolved.identity.to_str().ok_or_else(|| {
            Fail::usage("the recovery identity path must be UTF-8 to save it in config.local.toml")
        })?;
        set(
            &mut local.document,
            RECOVERY_IDENTITY,
            Value::from(recovery),
        )?;
    }
    let service = &["bootstrap", "services", "mise-history", "builtin"];
    if string(&local.document, service)?.is_some_and(|kind| kind != "history-watch") {
        return Err(Fail::usage(
            "bootstrap.services.mise-history already names a different service",
        ));
    }
    set(&mut local.document, service, Value::from("history-watch"))?;
    let settings = &[
        "bootstrap",
        "services",
        "mise-history",
        "environment",
        "MISE_CONFIG_DIR",
    ];
    let root = ctx.root.path().to_str().ok_or_else(|| {
        Fail::usage("the configuration root path must be UTF-8 to install the history watcher")
    })?;
    set(&mut local.document, settings, Value::from(root))?;
    set(
        &mut local.document,
        &["settings", "history", "sync"],
        Value::from("manual"),
    )?;

    // Native apply has no service selector. Load only the watcher declaration
    // for this invocation, so enrollment cannot reconcile unrelated services.
    let mut watcher = DocumentMut::new();
    let service_config = find(&local.document, &service[..3])
        .expect("watcher table")
        .clone();
    let mut services = Table::new();
    services.insert("mise-history", service_config);
    let mut bootstrap = Table::new();
    bootstrap.insert("services", Item::Table(services));
    watcher
        .as_table_mut()
        .insert("bootstrap", Item::Table(bootstrap));
    let scratch = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .map_err(watcher_error)?;
    let config = scratch.path().join("watcher.toml");
    let system = scratch.path().join("system.toml");
    fs::write(&config, watcher.to_string()).map_err(watcher_error)?;
    fs::write(&system, "").map_err(watcher_error)?;
    local.make_private();
    local.install()?;
    let mise = Mise::new(ctx)?;
    let mut apply = mise.command(["bootstrap", "services", "apply", "--yes"])?;
    apply
        .current_dir(scratch.path())
        .env(
            "MISE_CEILING_PATHS",
            scratch.path().parent().expect("scratch parent"),
        )
        .env("MISE_GLOBAL_CONFIG_FILE", config)
        .env("MISE_SYSTEM_CONFIG_FILE", system)
        .env("MISE_HISTORY_ENABLED", "0");
    if !run
        .run(stage, apply, Policy::Ordinary)
        .is_ok_and(|status| status.success())
    {
        return Err(Fail::new(1, "could not activate mise's history watcher"));
    }
    let selected = crate::config::selection::read(&ctx.root, &ctx.env)?;
    let mut status = mise.command(["dot", "status", "--json"])?;
    status.env("MISE_ENV", selected.active_environments());
    let output = run
        .probe_for(stage, status)
        .map_err(|_| Fail::new(1, "could not query mise's history watcher state"))?;
    if !output.status.success() {
        return Err(Fail::new(1, "could not query mise's history watcher state"));
    }
    let state: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| Fail::new(1, "mise returned an invalid history watcher status"))?;
    if state
        .pointer("/history/watcher")
        .and_then(serde_json::Value::as_str)
        != Some("running")
    {
        return Ok(super::defer(
            run,
            stage,
            "History enrolled, but the user watcher is not running.",
            "Use dots backup to save settings.",
        ));
    }
    Ok(Outcome::Completed)
}

fn machine_identity(ctx: &Ctx, run: &Run<'_>) -> Result<PathBuf, Fail> {
    use crate::process::Cwd;
    use std::fs::{DirBuilder, Permissions};
    use std::io::Write as _;
    use std::os::unix::fs::DirBuilderExt as _;

    let home = ctx
        .env
        .home()
        .ok_or_else(|| Fail::usage("HOME is needed to prepare the machine age identity"))?;
    let path = home.join(".config/age/keys.txt");
    if let Some(existing) = readable_identity(ctx, &path)? {
        return Ok(existing);
    }
    let directory = path.parent().expect("identity directory");
    let physical_home = fs::canonicalize(home).map_err(key_file_error)?;
    let physical_dir = crate::config::physical(directory).map_err(key_file_error)?;
    if !physical_dir.starts_with(&physical_home) || physical_dir.starts_with(ctx.root.path()) {
        return Err(Fail::usage(
            "the machine age identity must be inside HOME and outside the configuration root",
        ));
    }

    let command = ctx.command("age-keygen", Cwd::Root)?;
    let generated = run.probe(command).map_err(age_keygen_error)?;
    if !generated.status.success()
        || !generated
            .stdout
            .split(|byte| *byte == b'\n')
            .any(|line| line.starts_with(b"AGE-SECRET-KEY-"))
    {
        return Err(Fail::new(1, "could not generate the machine age identity"));
    }

    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&physical_dir)
        .map_err(key_file_error)?;
    let mut pending = tempfile::NamedTempFile::new_in(&physical_dir).map_err(key_file_error)?;
    pending
        .as_file()
        .set_permissions(Permissions::from_mode(0o600))
        .map_err(key_file_error)?;
    pending
        .write_all(&generated.stdout)
        .map_err(key_file_error)?;
    match pending.persist_noclobber(physical_dir.join("keys.txt")) {
        Ok(_) => {}
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(key_file_error(error.error)),
    }
    readable_identity(ctx, &path)?
        .ok_or_else(|| Fail::new(1, "the machine age identity could not be installed"))
}

fn key_file_error(error: io::Error) -> Fail {
    Fail::new(
        1,
        format!("could not install the machine age identity: {error}"),
    )
}

fn age_keygen_error(error: io::Error) -> Fail {
    if error.kind() == io::ErrorKind::NotFound {
        Fail::new(1, "age-keygen is required for encrypted history enrollment")
    } else {
        Fail::new(
            1,
            format!("could not run age-keygen for encrypted history enrollment: {error}"),
        )
    }
}

fn watcher_error(error: io::Error) -> Fail {
    Fail::new(
        1,
        format!("could not prepare the mise history watcher: {error}"),
    )
}
