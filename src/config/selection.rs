use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use toml_edit::{Array, DocumentMut, Item, Table, TableLike, Value};

use super::{Doc, Root, file_error, passive, valid_id};
use crate::process;
use crate::{Env, Fail};

const REPAIR: &str = "dots profile --profile PROFILE --machine-id ID";

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub profile: Option<String>,
    pub machine_id: Option<String>,
    pub selectors: Vec<String>,
}

impl Selection {
    pub fn environments(&self) -> String {
        self.profile
            .iter()
            .cloned()
            .chain(self.machine_id.iter().map(|id| format!("host-{id}")))
            .collect::<Vec<_>>()
            .join(",")
    }

    pub(crate) fn active_environments(&self) -> String {
        let primary = self.environments();
        std::iter::once(primary.as_str())
            .filter(|env| !env.is_empty())
            .chain(self.selectors.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(",")
    }
}

pub fn read(root: &Root, env: &Env) -> Result<Selection, Fail> {
    let profiles = passive::profiles(root)?;
    let mut selected = persisted(root, &profiles)?;
    if let Some(profile) = env.get("SETUP_PROFILE") {
        let profile = profile.to_string_lossy();
        check_profile(&profiles, &profile, Some("SETUP_PROFILE"))?;
        selected.profile = Some(profile.into_owned());
    }
    if let Some(machine) = env.get("SETUP_MACHINE_ID") {
        let machine = machine.to_string_lossy();
        check_machine(&machine, Some("SETUP_MACHINE_ID"))?;
        selected.machine_id = Some(machine.into_owned());
    }
    Ok(selected)
}

pub(crate) fn configured_profile(root: &Root) -> Result<Option<String>, Fail> {
    // A configuration command can start with no profile state at all.
    if !root
        .path()
        .join("miserc.toml")
        .try_exists()
        .map_err(|e| file_error(root.path(), e))?
        && !root
            .path()
            .join("config.local.toml")
            .try_exists()
            .map_err(|e| file_error(root.path(), e))?
    {
        return Ok(None);
    }
    let profiles = passive::profiles(root)?;
    Ok(persisted(root, &profiles)?.profile)
}

/// The native bootstrap task is the only caller that may use the ambient
/// MISE_ENV. An existing machine always retains its selection.
pub fn for_bootstrap(root: &Root, env: &Env, restore: Option<&str>) -> Result<Selection, Fail> {
    let profiles = passive::profiles(root)?;
    let mut selected = persisted(root, &profiles)?;
    if selected.profile.is_none() {
        if let Some(profile) = env.get("SETUP_PROFILE") {
            let profile = profile.to_string_lossy();
            check_profile(&profiles, &profile, Some("SETUP_PROFILE"))?;
            selected.profile = Some(profile.into_owned());
        } else if let Some(selectors) = env.get("MISE_ENV") {
            let matches: BTreeSet<_> = selectors
                .to_str()
                .unwrap_or_default()
                .split(',')
                .filter(|candidate| profiles.iter().any(|profile| profile == candidate))
                .collect();
            if matches.len() > 1 {
                return Err(Fail::usage("MISE_ENV selects more than one setup profile"));
            }
            selected.profile = matches.first().map(|p| (*p).to_owned());
        }
    }
    if let Some(restore) = restore {
        check_machine(restore, None)?;
        if selected
            .machine_id
            .as_deref()
            .is_some_and(|id| id != restore)
        {
            return Err(Fail::usage(
                "restore identity differs from this enrolled machine; use a fresh setup for replacement-machine recovery",
            ));
        }
        selected.machine_id = Some(restore.to_owned());
    }
    if selected.machine_id.is_none()
        && let Some(id) = env.get("SETUP_MACHINE_ID")
    {
        let id = id.to_string_lossy();
        check_machine(&id, Some("SETUP_MACHINE_ID"))?;
        selected.machine_id = Some(id.into_owned());
    }
    Ok(selected)
}

/// Init keeps valid persisted components and defaults only the missing or
/// invalid ones. Conflicting persisted choices still require explicit repair.
pub(crate) fn read_for_init(root: &Root) -> Result<Selection, Fail> {
    let profiles = passive::profiles(root)?;
    let mut selected = persisted_components(root, &profiles, true)?;
    selected.profile = selected
        .profile
        .filter(|profile| profiles.contains(profile));
    selected.machine_id = selected.machine_id.filter(|id| valid_id(id));
    Ok(selected)
}

fn persisted(root: &Root, profiles: &[String]) -> Result<Selection, Fail> {
    let selected = persisted_components(root, profiles, false)?;
    let profile = selected.profile.as_deref();
    if let Some(profile) = profile
        && !profiles.iter().any(|name| name == profile)
    {
        return Err(Fail::usage(format!(
            "selected profile '{profile}' is not in dots_profiles ({}); fix it with {REPAIR}",
            profiles.join(", ")
        )));
    }
    let machine = selected.machine_id.as_deref();
    if let Some(machine) = machine
        && !valid_id(machine)
    {
        return Err(Fail::usage(format!(
            "selected machine ID '{machine}' is invalid; fix it with {REPAIR}"
        )));
    }
    Ok(selected)
}

fn persisted_components(
    root: &Root,
    profiles: &[String],
    allow_invalid_components: bool,
) -> Result<Selection, Fail> {
    let early_path = root.path().join("miserc.toml");
    let local_path = root.path().join("config.local.toml");
    let early = passive::optional_toml(&early_path)?;
    let local = passive::optional_toml(&local_path)?;
    let selectors = selectors(&early, &early_path)?;
    let roles: BTreeSet<_> = selectors
        .iter()
        .filter(|selector| profiles.contains(selector))
        .collect();
    let host_ids: BTreeSet<_> = selectors
        .iter()
        .filter_map(|selector| selector.strip_prefix("host-"))
        .collect();
    for (kind, values) in [
        (
            "profile",
            roles.iter().map(|value| value.as_str()).collect::<Vec<_>>(),
        ),
        ("machine", host_ids.iter().copied().collect()),
    ] {
        if values.len() > 1 {
            return Err(Fail::usage(format!(
                "{} selects more than one {kind} ({}); choose one with {REPAIR}",
                early_path.display(),
                values.join(", ")
            )));
        }
    }
    let role = roles.first().map(|value| value.as_str());
    let host = host_ids.first().copied();
    let local_role = local_value(
        &local,
        &local_path,
        "SETUP_PROFILE",
        allow_invalid_components,
    )?;
    let local_host = local_value(
        &local,
        &local_path,
        "SETUP_MACHINE_ID",
        allow_invalid_components,
    )?;
    for (kind, early, local) in [
        ("profile", role, local_role),
        ("machine ID", host, local_host),
    ] {
        if let (Some(early), Some(local)) = (early, local)
            && early != local
        {
            return Err(Fail::usage(format!(
                "{} selects {kind} {} but {} records {}; choose one with {REPAIR}",
                early_path.display(),
                early,
                local_path.display(),
                local
            )));
        }
    }
    let extras = selectors
        .iter()
        .filter(|selector| !selector.starts_with("host-") && !profiles.contains(selector))
        .cloned()
        .collect();
    Ok(Selection {
        profile: role.or(local_role).map(str::to_owned),
        machine_id: host.or(local_host).map(str::to_owned),
        selectors: extras,
    })
}

fn local_value<'a>(
    local: &'a DocumentMut,
    path: &Path,
    key: &str,
    allow_invalid: bool,
) -> Result<Option<&'a str>, Fail> {
    let Some(item) = local
        .get("env")
        .and_then(Item::as_table_like)
        .and_then(|env| env.get(key))
    else {
        return Ok(None);
    };
    match item.as_str() {
        Some(value) => Ok((!value.is_empty()).then_some(value)),
        None if allow_invalid => Ok(None),
        None => Err(file_error(
            path,
            format_args!("env.{key} must be a string; fix it with {REPAIR}"),
        )),
    }
}

fn selectors(early: &DocumentMut, path: &Path) -> Result<Vec<String>, Fail> {
    let Some(item) = early.get("env") else {
        return Ok(Vec::new());
    };
    let invalid = || file_error(path, "env must be an array of selector names");
    item.as_array()
        .ok_or_else(invalid)?
        .iter()
        .map(|item| item.as_str().map(str::to_owned).ok_or_else(invalid))
        .collect()
}

pub struct Persist<'a> {
    pub profile: &'a str,
    pub machine_id: &'a str,
    pub origin: Option<&'a str>,
}

pub fn persist(root: &Root, env: &Env, selection: Persist<'_>) -> Result<(), Fail> {
    let Persist {
        profile,
        machine_id,
        origin,
    } = selection;
    let profiles = passive::profiles(root)?;
    check_profile(&profiles, profile, None)?;
    check_machine(machine_id, None)?;
    if let Some(origin) = origin {
        super::RepoSetting::parse(origin)
            .map_err(|error| Fail::usage(format!("--origin: {error}")))?;
    }
    let host = format!("host-{machine_id}");
    let mut early = Doc::open(&root.path().join("miserc.toml"))?;
    let mut local = Doc::open(&root.path().join("config.local.toml"))?;
    let shared_inode = match (fs::metadata(early.path()), fs::metadata(local.path())) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    if early.path() == local.path() || shared_inode {
        return Err(Fail::usage(
            "config.local.toml and miserc.toml must be separate files",
        ));
    }

    let old = selectors(&early.document, early.path())?;
    let mut next = vec![profile.to_owned(), host.clone()];
    for selector in &old {
        if !selector.is_empty()
            && !profiles.contains(selector)
            && !selector.starts_with("host-")
            && !next.contains(selector)
        {
            next.push(selector.clone());
        }
    }
    if old != next {
        set_selectors(&mut early, next);
    }
    set_string(&mut local, &["env", "SETUP_PROFILE"], profile)?;
    set_string(&mut local, &["env", "SETUP_MACHINE_ID"], machine_id)?;
    set_string(&mut local, &["settings", "history", "sync"], "manual")?;
    if let Some(origin) = origin {
        set_string(&mut local, &["env", "SETUP_HISTORY_ORIGIN"], origin)?;
        set_string(&mut local, &["history", "origin", "url"], origin)?;
        set_string(&mut local, &["history", "origin", "branch"], &host)?;
    }

    early.make_private();
    local.make_private();
    let early = early.stage()?;
    let local = local.stage()?;
    process::validate_selection(
        env,
        early.file.as_file(),
        local.file.as_file(),
        profile,
        &host,
    )?;
    local.check_destination()?;
    early.check_destination()?;
    local.install()?;
    early.install()
}

fn set_selectors(early: &mut Doc, next: Vec<String>) {
    let mut replacement: Array = next.into_iter().collect();
    if let Some(previous) = early.document.get("env").and_then(Item::as_array) {
        let mut comments = String::new();
        for value in previous.iter() {
            append_comments(
                &mut comments,
                value.decor().prefix().and_then(|s| s.as_str()),
            );
            append_comments(
                &mut comments,
                value.decor().suffix().and_then(|s| s.as_str()),
            );
        }
        append_comments(&mut comments, previous.trailing().as_str());
        append_comments(
            &mut comments,
            previous.decor().suffix().and_then(|s| s.as_str()),
        );
        if let Some(prefix) = previous.decor().prefix().and_then(|s| s.as_str()) {
            replacement.decor_mut().set_prefix(prefix.to_owned());
        }
        if !comments.is_empty()
            && let Some(mut key) = early.document.key_mut("env")
        {
            let prefix = key
                .leaf_decor()
                .prefix()
                .and_then(|s| s.as_str())
                .unwrap_or_default();
            let prefix = format!("{prefix}{comments}");
            key.leaf_decor_mut().set_prefix(prefix);
        }
    }
    early.document["env"] = Item::Value(Value::Array(replacement));
}

fn append_comments(output: &mut String, decoration: Option<&str>) {
    for line in decoration.unwrap_or_default().lines() {
        if let Some((_, comment)) = line.split_once('#') {
            output.push('#');
            output.push_str(comment);
            output.push('\n');
        }
    }
}

fn set_string(doc: &mut Doc, path: &[&str], value: &str) -> Result<(), Fail> {
    let [parents @ .., key] = path else {
        unreachable!()
    };
    let label = doc.path().to_path_buf();
    let mut table: &mut dyn TableLike = doc.document.as_table_mut();
    let mut inline = false;
    for (index, parent) in parents.iter().enumerate() {
        if !table.contains_key(parent) {
            let mut child = Table::new();
            child.set_implicit(index + 1 < parents.len());
            table.insert(parent, Item::Table(child));
        }
        let child = table.get_mut(parent).expect("inserted table");
        inline = child.is_inline_table() || child.as_table().is_some_and(Table::is_dotted);
        table = child.as_table_like_mut().ok_or_else(|| {
            file_error(
                &label,
                format_args!("{} is not a table", parents[..=index].join(".")),
            )
        })?;
    }
    if table.get(key).and_then(Item::as_str) == Some(value) {
        return Ok(());
    }
    let mut replacement = Value::from(value);
    if let Some(previous) = table.get(key).and_then(Item::as_value) {
        let decoration = previous.decor().clone();
        let mut comments = String::new();
        append_comments(&mut comments, decoration.suffix().and_then(|s| s.as_str()));
        if inline {
            *replacement.decor_mut() = decoration;
        } else if let Some(mut name) = table.key_mut(key) {
            let prefix = name
                .leaf_decor()
                .prefix()
                .and_then(|s| s.as_str())
                .unwrap_or_default();
            let prefix = format!("{prefix}{comments}");
            name.fmt();
            name.leaf_decor_mut().set_prefix(prefix);
        }
    }
    let item = Item::Value(replacement);
    if let Some(previous) = table.get_mut(key) {
        *previous = item;
    } else {
        table.insert(key, item);
    }
    Ok(())
}

fn check_profile(profiles: &[String], value: &str, source: Option<&str>) -> Result<(), Fail> {
    if profiles.iter().any(|profile| profile == value) {
        return Ok(());
    }
    let who = source.map_or_else(
        || "unknown profile".to_owned(),
        |source| format!("{source} names unknown profile"),
    );
    Err(Fail::usage(format!(
        "{who} '{value}'; profile must be one of: {}",
        profiles.join(", ")
    )))
}

fn check_machine(value: &str, source: Option<&str>) -> Result<(), Fail> {
    if valid_id(value) {
        return Ok(());
    }
    let problem = source.map_or_else(
        || format!("invalid machine ID '{value}'"),
        |source| format!("{source} '{value}' is not a valid machine ID"),
    );
    Err(Fail::usage(format!(
        "{problem}; use lowercase letters, digits and -"
    )))
}
