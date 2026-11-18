use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

use crate::process::Mise;
use crate::run::{Run, StageId};
use crate::targets::{Declaration, OwnedApp, Resolution, Scope, Section, Target, Targets};
use crate::{Args, Ctx, Fail};

use super::edit::{self, Edit, TOOLS, Written};
use super::{
    adopt, github, github_url, io_fail, native, scope, selected_path, software_request, stage,
    supervise,
};

pub fn run(ctx: &Ctx, args: &Args, run: &Run<'_>) -> Result<(), Fail> {
    let requests = args
        .get_many::<OsString>("TARGET")
        .expect("required by grammar");
    let scope = scope(args);
    let keep = args.get_flag("keep-installed");
    let dry = args.get_flag("dry-run");
    let caller = ctx.env.cwd();
    let selected = selected_path(ctx, &scope, false)?;
    let files = adopt::requests(ctx, selected.as_deref(), requests.clone())?;
    let has_software = files.iter().any(Option::is_none);
    let scope = match (&scope, &selected) {
        (Scope::Path(_), Some(path)) => Scope::Path(path.clone()),
        _ => scope,
    };
    let inventory = has_software
        .then(|| Targets::read(&ctx.root, scope.clone(), caller))
        .transpose()?;
    let active = (has_software && !keep && scope != Scope::Active)
        .then(|| Targets::read(&ctx.root, Scope::Active, caller))
        .transpose()?;

    let mut seen = BTreeSet::new();
    let mut plans = Vec::new();
    for (request, file) in requests.zip(files) {
        if let Some(file) = file {
            plans.push(Plan::File(adopt::prepare_remove(
                ctx,
                file,
                selected.as_deref(),
            )?));
            continue;
        }
        let request = software_request(request)?;
        let inventory = inventory.as_ref().expect("software requested");
        let target = resolve(inventory, active.as_ref(), request, keep)?;
        if let Some(recipe) = &target.held {
            return Err(Fail::new(1, super::held(&target.id, recipe).diagnostic));
        }
        if seen.insert(target.id.clone()) {
            plans.push(Plan::Software(Software::new(target, request)));
        }
    }

    let selected_declarations: Vec<_> = plans
        .iter()
        .flat_map(|plan| match plan {
            Plan::Software(software) => software.target.declarations.as_slice(),
            Plan::File(_) => &[],
        })
        .cloned()
        .collect();
    for plan in &mut plans {
        if let Plan::Software(plan) = plan
            && !keep
        {
            let active = active
                .as_ref()
                .or(inventory.as_ref())
                .expect("software inventory");
            plan.prepare(ctx, run, active, &selected_declarations)?;
        }
    }
    for plan in plans {
        stage(run, "Remove request", "dots remove", |id| match plan {
            Plan::File(plan) if dry => {
                plan.describe();
                Ok(())
            }
            Plan::File(plan) => plan.execute(ctx),
            Plan::Software(plan) if dry => {
                plan.describe();
                Ok(())
            }
            Plan::Software(plan) => plan.execute(ctx, run, id),
        })?;
    }
    Ok(())
}

enum Plan {
    File(adopt::Removal),
    Software(Software),
}

fn resolve(
    inventory: &Targets,
    active: Option<&Targets>,
    request: &str,
    keep: bool,
) -> Result<Target, Fail> {
    match inventory.resolve(request, keep) {
        Resolution::One(target) => Ok(target),
        Resolution::Ambiguous(targets) => {
            let alternatives: BTreeSet<_> = targets.iter().map(Target::qualified).collect();
            Err(Fail::new(
                1,
                format!(
                    "ambiguous target {request:?}; use one of: {}",
                    alternatives.into_iter().collect::<Vec<_>>().join(", ")
                ),
            ))
        }
        Resolution::None(suggestion) => {
            if !keep
                && (github_url(request) || request.starts_with("github:"))
                && let Some(error) = inventory
                    .ownership_error()
                    .or_else(|| active.and_then(Targets::ownership_error))
            {
                return Err(Fail::new(
                    1,
                    format!(
                        "cannot verify managed app ownership at {}: {}",
                        error.path.display(),
                        error.detail
                    ),
                ));
            }
            let mut message = format!("no declaration or managed app matches '{request}'");
            if let Some(name) = suggestion {
                message.push_str(&format!(". Did you mean '{name}'?"));
            }
            Err(Fail::new(1, message))
        }
    }
}

struct Software {
    target: Target,
    uninstall: bool,
    package: Option<String>,
    requested: String,
    any_installed_package: bool,
    prune: bool,
}

impl Software {
    fn new(target: Target, requested: &str) -> Self {
        Self {
            target,
            uninstall: false,
            package: None,
            requested: requested.into(),
            any_installed_package: false,
            prune: false,
        }
    }

    fn prepare(
        &mut self,
        ctx: &Ctx,
        run: &Run<'_>,
        active: &Targets,
        selected: &[Declaration],
    ) -> Result<(), Fail> {
        let target = &self.target;
        if active.get(&target.id).is_some_and(|matched| {
            matched
                .declarations
                .iter()
                .any(|entry| !selected.contains(entry))
        }) {
            return Ok(());
        }
        let native_keys: BTreeSet<_> = target
            .declarations
            .iter()
            .filter(|decl| decl.section == Section::Packages)
            .map(|decl| decl.key.as_str())
            .collect();
        let package = choose_native(&self.requested, &native_keys);
        if let Some(key) = package {
            for other in active
                .iter()
                .flat_map(|target| &target.declarations)
                .filter(|other| other.section == Section::Packages && !selected.contains(other))
            {
                if native::shares(ctx, run, key, &other.key)? {
                    return Ok(());
                }
            }
        }
        if !target.owned_apps.is_empty() && release_still_used(ctx, run, target)? {
            return Ok(());
        }
        if let Some(error) = target.ownership_error.as_ref().or_else(|| {
            active
                .get(&target.id)
                .and_then(|target| target.ownership_error.as_ref())
        }) {
            return Err(Fail::new(
                1,
                format!(
                    "cannot verify managed app ownership at {}: {}",
                    error.path.display(),
                    error.detail
                ),
            ));
        }
        for app in &target.owned_apps {
            github::validate_owned_app(ctx, app)?;
        }

        self.uninstall = true;
        if let Some(key) = package {
            self.any_installed_package = native::plan(ctx, run, key)?.is_some();
            self.package = Some(key.to_owned());
        }
        self.prune = !target.owned_apps.is_empty()
            || target
                .declarations
                .iter()
                .any(|decl| decl.section == Section::Tools);
        if self.prune {
            Mise::new(ctx)?;
        }
        Ok(())
    }

    fn describe(&self) {
        let verb = if !self.uninstall {
            "Forget"
        } else if self.prune || self.any_installed_package {
            "Uninstall"
        } else {
            "Forget absent package"
        };
        println!("{verb} {}", self.target.id);
        for declaration in &self.target.declarations {
            println!(
                "  Remove {} from {}",
                declaration.key,
                declaration.path.display()
            );
        }
        if self.uninstall {
            for app in &self.target.owned_apps {
                println!("  Move {} to Trash", app.path.display());
            }
        }
    }

    fn execute(self, ctx: &Ctx, run: &Run<'_>, id: StageId) -> Result<(), Fail> {
        let mut edits = BTreeMap::new();
        for declaration in &self.target.declarations {
            if !edits.contains_key(&declaration.path) {
                edits.insert(declaration.path.clone(), Edit::open(&declaration.path)?);
            }
        }
        let mise = self.prune.then(|| Mise::new(ctx)).transpose()?;
        let mut apps = Vec::new();
        let mut written: Vec<Written> = Vec::new();
        let result = (|| {
            if let Some(mise) = &mise {
                for declaration in self
                    .target
                    .declarations
                    .iter()
                    .filter(|decl| decl.section == Section::Tools)
                {
                    unuse(
                        ctx,
                        run,
                        id,
                        mise,
                        &edits[&declaration.path].doc.document,
                        declaration,
                    )?;
                }
            }
            for declaration in &self.target.declarations {
                let edit = edits
                    .get_mut(&declaration.path)
                    .expect("edit per declaration");
                let section = edit::section(declaration);
                if edit::entry(&edit.doc.document, section, &declaration.key)?.is_none() {
                    return Err(Fail::new(
                        1,
                        format!("{} changed since discovery", declaration.path.display()),
                    ));
                }
                edit::delete(&mut edit.doc.document, section, &declaration.key)?;
            }
            // Finish reversible writes before asking a native manager to
            // change the machine. A failure restores these writes in reverse.
            for edit in edits.into_values() {
                if let Some(edit) = edit.install()? {
                    written.push(edit);
                }
            }
            if self.uninstall {
                for app in &self.target.owned_apps {
                    apps.push(StagedApp::new(ctx, app)?);
                }
                if let Some(key) = &self.package
                    && let Some(command) = native::plan(ctx, run, key)?
                {
                    supervise(run, id, command, &format!("uninstall {key}"))?;
                }
            }
            if let Some(mise) = mise {
                supervise(
                    run,
                    id,
                    mise.command(["prune", "--yes", "--tools", "--", &self.target.id])?,
                    &format!("prune {}", self.target.id),
                )?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            return Err(edit::rollback(
                error,
                written
                    .into_iter()
                    .rev()
                    .map(|written| written.restore_declarations(&self.target.declarations))
                    .chain(apps.into_iter().rev().map(StagedApp::restore)),
            ));
        }
        for app in apps {
            app.finish();
        }
        Ok(())
    }
}

fn choose_native<'a>(requested: &'a str, keys: &BTreeSet<&'a str>) -> Option<&'a str> {
    let first = keys.first().copied()?;
    if let Ok((manager, name)) = native::split(requested)
        && matches!(manager, "brew" | "brew-cask")
        && name.contains('/')
        && native::split(first).is_ok_and(|(kind, _)| kind == manager)
    {
        return Some(requested);
    }
    // A qualified declaration carries tap information that the inventory's
    // canonical key intentionally drops for built-in Homebrew taps.
    keys.iter()
        .copied()
        .find(|key| key.contains('/'))
        .or(Some(first))
}

fn release_still_used(ctx: &Ctx, run: &Run<'_>, target: &Target) -> Result<bool, Fail> {
    #[derive(serde::Deserialize)]
    struct Version {
        #[serde(default)]
        sources: Vec<Source>,
    }

    #[derive(serde::Deserialize)]
    struct Source {
        path: PathBuf,
    }

    let problem = || {
        Fail::new(
            1,
            format!("cannot verify the tracked projects using {}", target.id),
        )
    };
    let mise = Mise::new(ctx)?;
    let output = run
        .probe(mise.command(["ls", &target.id, "--json", "--all-sources"])?)
        .map_err(|_| problem())?;
    if !output.status.success() {
        return Err(problem());
    }
    let versions: Vec<Version> = serde_json::from_slice(&output.stdout).map_err(|_| problem())?;
    for source in versions.into_iter().flat_map(|version| version.sources) {
        if !source.path.is_absolute() {
            return Err(problem());
        }
        let path = super::physical(&source.path).map_err(|_| problem())?;
        if !target.declarations.iter().any(|entry| entry.path == path) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn unuse(
    ctx: &Ctx,
    run: &Run<'_>,
    stage: StageId,
    mise: &Mise<'_>,
    source: &DocumentMut,
    decl: &Declaration,
) -> Result<(), Fail> {
    let current = edit::entry(source, TOOLS, &decl.key)?.ok_or_else(|| {
        Fail::new(
            1,
            format!("{} changed since discovery", decl.path.display()),
        )
    })?;
    let mut document = DocumentMut::new();
    edit::put(&mut document, TOOLS, &decl.key, current.clone())?;
    let dir = tempfile::tempdir().map_err(|error| io_fail("could not stage", &decl.path, error))?;
    let config = dir.path().join("mise.toml");
    fs::write(&config, document.to_string())
        .map_err(|error| io_fail("could not stage", &decl.path, error))?;
    let mut command = mise.command([
        OsStr::new("unuse"),
        OsStr::new("--path"),
        config.as_os_str(),
        OsStr::new("--no-prune"),
        OsStr::new("--"),
        OsStr::new(&decl.key),
    ])?;
    super::add::trust_stage(ctx, &mut command, dir.path())?;
    supervise(run, stage, command, &format!("unuse {}", decl.key))?;
    let changed =
        fs::read_to_string(&config).map_err(|error| io_fail("could not read", &config, error))?;
    let changed: DocumentMut = changed
        .parse()
        .map_err(|_| Fail::new(1, "mise unuse wrote invalid TOML"))?;
    if edit::entry(&changed, TOOLS, &decl.key)?.is_some() {
        return Err(Fail::new(
            1,
            format!("mise unuse did not remove {}", decl.key),
        ));
    }
    Ok(())
}

struct StagedApp {
    path: PathBuf,
    moved: Option<(tempfile::TempDir, PathBuf)>,
    marker: adopt::Stashed,
}

impl StagedApp {
    fn new(ctx: &Ctx, app: &OwnedApp) -> Result<Self, Fail> {
        let marker = adopt::needed(&app.marker)?;
        let inspect = |path: &Path| {
            adopt::snapshot(path).map_err(|error| io_fail("could not inspect", path, error))
        };
        let original = inspect(&app.path)?;
        github::validate_owned_app(ctx, app)?;
        let changed = || {
            Fail::new(
                1,
                format!(
                    "{} changed after validating its ownership",
                    app.path.display()
                ),
            )
        };
        if inspect(&app.path)? != original {
            return Err(changed());
        }
        let moved = if let Some(original) = original {
            let home = ctx
                .env
                .home()
                .ok_or_else(|| Fail::new(1, "set HOME to find the Trash"))?;
            let (work, destination) = move_to_trash(home, &app.path)?;
            if !inspect(&destination).is_ok_and(|entry| entry == Some(original)) {
                return Err(edit::rollback(
                    changed(),
                    [restore_app(work, &app.path, &destination)],
                ));
            }
            Some((work, destination))
        } else {
            None
        };
        let marker = match adopt::Stashed::new(&app.marker, &marker) {
            Ok(marker) => marker,
            Err(error) => {
                return Err(edit::rollback(
                    error,
                    moved.map(|(work, destination)| restore_app(work, &app.path, &destination)),
                ));
            }
        };
        Ok(Self {
            path: app.path.clone(),
            moved,
            marker,
        })
    }

    fn restore(self) -> Result<(), Fail> {
        let app = self.moved.map_or(Ok(()), |(work, destination)| {
            restore_app(work, &self.path, &destination)
        });
        let marker = self.marker.restore();
        match (app, marker) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(app), Err(marker)) => Err(edit::rollback(app, [Err(marker)])),
        }
    }

    fn finish(self) {
        if let Some((work, _)) = self.moved {
            let _ = work.keep();
        }
    }
}

fn move_to_trash(home: &Path, path: &Path) -> Result<(tempfile::TempDir, PathBuf), Fail> {
    let trash = home.join(".Trash");
    match fs::DirBuilder::new().mode(0o700).create(&trash) {
        Ok(()) => (),
        Err(error)
            if error.kind() == std::io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(&trash).is_ok_and(|metadata| metadata.is_dir()) => {}
        Err(error) => return Err(io_fail("could not open Trash", &trash, error)),
    }
    let work = tempfile::Builder::new()
        .prefix("dots-")
        .tempdir_in(&trash)
        .map_err(|error| io_fail("could not prepare Trash", &trash, error))?;
    let destination = work.path().join(
        path.file_name()
            .ok_or_else(|| Fail::new(1, "invalid app path"))?,
    );
    fs::rename(path, &destination)
        .map_err(|error| io_fail("could not move app to Trash", path, error))?;
    Ok((work, destination))
}

fn restore_app(work: tempfile::TempDir, path: &Path, in_trash: &Path) -> Result<(), Fail> {
    if let Err(error) = adopt::rename_noclobber(in_trash, path) {
        let _ = work.keep();
        return Err(Fail::new(
            1,
            format!(
                "could not restore {}: {error}; app remains at {}",
                path.display(),
                in_trash.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_move_preserves_the_bundle_and_rollback_preserves_a_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let app = temp.path().join("fixture.app");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&app).unwrap();
        fs::write(app.join("fixture"), "original").unwrap();

        let (trash, parked) = move_to_trash(&home, &app).unwrap();
        assert!(parked.starts_with(home.join(".Trash")));
        assert!(!app.exists());
        assert_eq!(
            fs::read_to_string(parked.join("fixture")).unwrap(),
            "original"
        );
        restore_app(trash, &app, &parked).unwrap();

        let (trash, parked) = move_to_trash(&home, &app).unwrap();
        fs::create_dir(&app).unwrap();
        fs::write(app.join("fixture"), "later").unwrap();
        assert!(restore_app(trash, &app, &parked).is_err());
        assert_eq!(fs::read_to_string(app.join("fixture")).unwrap(), "later");
        assert_eq!(
            fs::read_to_string(parked.join("fixture")).unwrap(),
            "original"
        );
    }
}
