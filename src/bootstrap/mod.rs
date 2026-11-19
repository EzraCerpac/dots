mod bundle;
pub(crate) mod bundle_pipe;
mod private;
mod source;

pub use bundle::bundle;

use std::ffi::{OsStr, OsString};
use std::fs::{File, Permissions};
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use crate::config::{Exceptions, Root, Vars, selection};
use crate::exceptions;
use crate::history::{self, Restore};
use crate::hooks::{self, HookEnv};
use crate::process::Mise;
use crate::report::{self, Command as ReportCommand};
use crate::run::{Issue, Outcome, Policy, Run, Sink, StageId};
use crate::{Args, Ctx, Env, Fail};

const BUNDLE: &str = "DOTS_BOOTSTRAP_BUNDLE";
const IDENTITY: &str = "DOTS_BOOTSTRAP_IDENTITY";
const RESTORE: &str = "DOTS_BOOTSTRAP_RESTORE";
const STAGES: &str = "DOTS_STAGES";
const HISTORY_ENABLED: &str = "MISE_HISTORY_ENABLED";
const PARENT_HISTORY_ENABLED: &str = "DOTS_BOOTSTRAP_PARENT_HISTORY_ENABLED";
const FRAGMENT: &str = "stages.json";
const RETRY: &str = "Resolve the issue and rerun dots bootstrap.";

#[derive(Default)]
struct Request {
    bundle: Option<PathBuf>,
    identity: Option<PathBuf>,
    restore: Option<String>,
}

pub fn bootstrap(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    match args.subcommand_name() {
        Some("persist-selected") => persist_selected(ctx),
        Some("complete") => complete(ctx),
        None => launch(ctx, args),
        _ => unreachable!("selected by CLI"),
    }
}

fn arg_path(ctx: &Ctx, args: &Args, name: &str) -> Option<PathBuf> {
    args.get_one::<OsString>(name)
        .map(|path| ctx.env.cwd().join(path))
}

fn env_string<'a>(ctx: &'a Ctx, key: &str) -> Result<Option<&'a str>, Fail> {
    ctx.env
        .get(key)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| Fail::usage(format!("{key} must be UTF-8")))
        })
        .transpose()
}

fn callback_request(ctx: &Ctx) -> Result<Request, Fail> {
    Ok(Request {
        bundle: ctx.env.get(BUNDLE).map(|path| ctx.env.cwd().join(path)),
        identity: ctx.env.get(IDENTITY).map(|path| ctx.env.cwd().join(path)),
        restore: env_string(ctx, RESTORE)?.map(str::to_owned),
    })
}

fn auto_id() -> Result<String, Fail> {
    let mut random = [0; 16];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .map_err(|error| Fail::new(1, format!("could not generate a machine ID: {error}")))?;
    Ok(format!("auto-{:032x}", u128::from_ne_bytes(random)))
}

fn persist_selected(ctx: &Ctx) -> Result<u8, Fail> {
    let restore = env_string(ctx, RESTORE)?;
    let selected = selection::for_bootstrap(&ctx.root, &ctx.env, restore)?;
    let profile = selected
        .profile
        .ok_or_else(|| Fail::usage("select a bootstrap profile with SETUP_PROFILE or MISE_ENV"))?;
    let machine = selected.machine_id.map_or_else(auto_id, Ok)?;
    let origin = private::persistence_origin(ctx)?;
    selection::persist(
        &ctx.root,
        &ctx.env,
        selection::Persist {
            profile: &profile,
            machine_id: &machine,
            origin: origin.as_deref(),
        },
    )?;
    println!("Selected mise profile {profile} for host {machine}.");
    Ok(0)
}

fn append_strings(native: &mut Vec<OsString>, args: &Args, key: &str) {
    if let Some(values) = args.get_many::<String>(key) {
        for value in values {
            native.extend([format!("--{key}").into(), value.into()]);
        }
    }
}

fn append_flags(native: &mut Vec<OsString>, args: &Args, keys: &[&str]) {
    native.extend(
        keys.iter()
            .filter(|key| args.get_flag(key))
            .map(|key| OsString::from(format!("--{key}"))),
    );
}

fn native_args(args: &Args, restore: Option<&str>) -> Vec<OsString> {
    let mut native = vec!["bootstrap".into(), "--skip-dirty".into()];
    append_flags(
        &mut native,
        args,
        &[
            "replace-history",
            "take-remote-all",
            "dry-run",
            "yes",
            "force-dotfiles",
            "prune-dotfiles",
            "prompt-secrets",
            "prompt-vars",
            "save-env",
            "update",
            "upgrade",
        ],
    );
    for key in ["config-dir", "only", "skip"] {
        append_strings(&mut native, args, key);
    }
    if let Some(path) = args.get_one::<OsString>("from-dir") {
        native.extend(["--from-dir".into(), path.clone()]);
    }
    match (restore, args.get_many::<String>("env")) {
        (Some(machine), Some(environments)) => {
            for environment in environments.filter(|name| !name.starts_with("host-")) {
                native.extend(["--env".into(), environment.into()]);
            }
            native.extend(["--env".into(), format!("host-{machine}").into()]);
        }
        _ => append_strings(&mut native, args, "env"),
    }
    if restore.is_some()
        && !args
            .get_many::<String>("skip")
            .is_some_and(|mut parts| parts.any(|part| part == "services"))
    {
        native.extend(["--skip".into(), "services".into()]);
    }
    native
}

fn launch(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let run = Run::begin(ctx, Sink::Report(ReportCommand::Bootstrap));
    let restore = args.get_one::<String>("restore").map(String::as_str);
    let mut native = native_args(args, restore);
    let vars = Vars::read(&ctx.root)?;
    let from = args.get_one::<String>("from");
    let adopt = args.get_one::<String>("adopt");
    if from.is_some() && adopt.is_some() {
        return Err(Fail::usage("choose either --from or --adopt"));
    }
    let source = [("from", from), ("adopt", adopt)]
        .into_iter()
        .find_map(|(key, value)| value.map(|value| (key, value)));
    let source = source
        .map(|(key, value)| {
            source::validate_native_source(ctx, &run, value, &vars).map(|url| (key, url))
        })
        .transpose()?;
    if args.get_flag("update") {
        source::validate_update(
            ctx,
            &run,
            &vars,
            source.as_ref(),
            args.get_one::<OsString>("from-dir"),
        )?;
    }
    if let Some((key, source)) = source {
        native.extend([format!("--{key}").into(), source.argument().into()]);
    }

    // Only persist-selected reads the MISE_ENV supplied by its own caller.
    // An outer bootstrap must not pass the caller's project selector to mise.
    let restore_selection = restore
        .map(|machine| {
            let mut vars = ctx.env.vars.clone();
            vars.remove(std::ffi::OsStr::new("MISE_ENV"));
            let env = Env {
                vars,
                cwd: ctx.env.cwd().to_owned(),
            };
            selection::for_bootstrap(&ctx.root, &env, Some(machine))
        })
        .transpose()?;
    let selection = match restore_selection {
        Some(selection) => selection,
        None => selection::read(&ctx.root, &ctx.env)?,
    };
    let profile = selection.profile.clone();
    let mut command = Mise::with_selection(ctx, selection)?.command(native)?;
    source::clear_git_checkout_override(&mut command);
    let mut parent_history_enabled = OsString::new();
    if let Some(value) = ctx.env.get(HISTORY_ENABLED) {
        parent_history_enabled.push("=");
        parent_history_enabled.push(value);
    }
    // Native bootstrap owns installation. Its pre-dotfiles callback may use
    // mise x to install the pinned dots before the rest of the declared tools.
    command
        .env_remove("MISE_AUTO_INSTALL")
        .env_remove("MISE_EXEC_AUTO_INSTALL")
        // Keep mise and the Git preflight rooted in the same checkout context.
        .env_remove("MISE_CD")
        // Native checkpoints do not enforce dots' encrypted-history contract.
        .env(HISTORY_ENABLED, "0")
        .env(PARENT_HISTORY_ENABLED, parent_history_enabled);
    if let Some(machine) = restore {
        command.env("SETUP_MACHINE_ID", machine);
        if let Some(profile) = profile {
            command.env("SETUP_PROFILE", profile);
        }
    }
    for (key, value) in [
        (
            BUNDLE,
            arg_path(ctx, args, "bundle").map(PathBuf::into_os_string),
        ),
        (
            IDENTITY,
            arg_path(ctx, args, "identity").map(PathBuf::into_os_string),
        ),
        (RESTORE, restore.map(OsString::from)),
    ] {
        if let Some(value) = value {
            command.env(key, value);
        } else {
            command.env_remove(key);
        }
    }

    let fragment = tempfile::Builder::new()
        .prefix("dots-bootstrap-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
        .and_then(|temp| {
            let path = std::path::absolute(temp.path().join(FRAGMENT))?;
            Ok((temp, path))
        });
    if let Ok((_, path)) = &fragment {
        command.env(STAGES, path);
    } else if let Err(error) = &fragment {
        eprintln!("dots: could not prepare stage metadata for dots bootstrap: {error}");
    }
    let stage = run.start("Native mise bootstrap");
    let result = run.run(stage, command, Policy::Ordinary);
    let outcome = match result {
        _ if run.cancelled() => Outcome::Cancelled,
        Ok(status) if status.success() => Outcome::Completed,
        Ok(status) => {
            run.issue(
                stage,
                Issue::new(format!("native mise bootstrap exited with {status}"), RETRY),
            );
            Outcome::Failed
        }
        Err(error) => {
            let message = format!("could not run native mise bootstrap: {error}");
            eprintln!("dots: {message}");
            run.issue(stage, Issue::new(message, RETRY));
            Outcome::Failed
        }
    };
    run.finish(stage, outcome);
    if let Ok((_, path)) = &fragment {
        match run.fold(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if outcome == Outcome::Failed && !run.cancelled() {
                    eprintln!("dots: native bootstrap failed before dots bootstrap complete ran.");
                }
            }
            Err(error) => {
                eprintln!("dots: could not read stage metadata for dots bootstrap: {error}")
            }
        }
    }
    Ok(run.end(u8::from(outcome == Outcome::Failed)))
}

fn complete(ctx: &Ctx) -> Result<u8, Fail> {
    // Native bootstrap disables its own checkpoint. Explicit history commands
    // in this callback must inherit the caller's original setting.
    let callback;
    let ctx = if let Some(previous) = ctx.env.get(PARENT_HISTORY_ENABLED) {
        let mut vars = ctx.env.vars.clone();
        vars.remove(OsStr::new(PARENT_HISTORY_ENABLED));
        vars.remove(OsStr::new(HISTORY_ENABLED));
        if let Some(value) = previous.as_bytes().strip_prefix(b"=") {
            vars.insert(HISTORY_ENABLED.into(), OsStr::from_bytes(value).into());
        }
        let env = Env {
            vars,
            cwd: ctx.env.cwd().to_owned(),
        };
        callback = Ctx {
            root: Root::from_dir(&env, ctx.root.path())?,
            flags: ctx.flags,
            env,
        };
        &callback
    } else {
        ctx
    };
    let sink = match ctx.env.get(STAGES) {
        Some(path) => {
            let path = PathBuf::from(path);
            if path.file_name() != Some(FRAGMENT.as_ref())
                || !path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name.as_encoded_bytes().starts_with(b"dots-bootstrap-"))
            {
                return Err(Fail::usage("invalid native bootstrap stage fragment path"));
            }
            if let Err(error) = report::save_fragment(&path, &[]) {
                eprintln!(
                    "dots: could not start stage metadata for dots bootstrap complete: {error}"
                );
            }
            Sink::Fragment(path)
        }
        None => Sink::Unreported,
    };
    let run = Run::begin(ctx, sink);
    let request = callback_request(ctx)?;
    let vars = Vars::read(&ctx.root)?;
    let selected = selection::read(&ctx.root, &ctx.env)?;
    let (Some(profile), Some(machine)) = (selected.profile, selected.machine_id) else {
        return Err(Fail::usage(
            "persist the profile and machine ID before running dots bootstrap complete",
        ));
    };
    if let Some(id) = &request.restore {
        selection::for_bootstrap(&ctx.root, &ctx.env, Some(id))?;
        if id != &machine {
            return Err(Fail::usage(
                "the restore ID differs from the selected machine ID",
            ));
        }
    }

    let bundle = stage(&run, "Private inputs", |id| {
        private::open_bundle(ctx, &run, id, &request, &profile)
    });
    let bundle_ready = bundle
        .as_ref()
        .is_some_and(|(outcome, _)| *outcome == Outcome::Completed);
    let bundle = bundle.and_then(|(_, bundle)| bundle);

    stage(&run, "Source repository", |_| {
        source::prepare(ctx, &run, &vars).map(|outcome| (outcome, ()))
    });
    let restored = request.restore.is_some()
        && stage(&run, "Restore private settings", |id| {
            let Some(resolved) = private::resolve_history(ctx, &request, &vars, &machine, true)?
            else {
                return Err(Fail::new(
                    1,
                    "restoring private settings requires an existing external age identity",
                ));
            };
            let outcome = history::restore(
                &run,
                id,
                Restore {
                    machine: &machine,
                    origin: &resolved.origin,
                    identity: &resolved.identity,
                    initialize: true,
                },
            );
            Ok((outcome, ()))
        })
        .is_some_and(|(outcome, ())| outcome == Outcome::Completed);

    let ready = bundle_ready && (request.restore.is_none() || restored);
    let json = if ready {
        bundle
            .as_ref()
            .map(bundle::Bundle::secrets_json)
            .transpose()?
    } else {
        None
    };
    if !run.cancelled() {
        match Exceptions::read(&ctx.root)
            .and_then(|settings| exceptions::run(&run, &settings, exceptions::Mode::InstallOnly))
        {
            Ok(_) => {}
            Err(error) => {
                stage::<()>(&run, "Installer exceptions", |_| Err(error));
            }
        }
    }
    if !run.cancelled() {
        match hooks::list(&ctx.root, hooks::Dir::Bootstrap, &vars, &profile) {
            Ok(hooks) => {
                for hook in &hooks {
                    if run.cancelled() {
                        break;
                    }
                    hooks::run(
                        &run,
                        hook,
                        HookEnv::Bootstrap {
                            private_ready: ready,
                            restored,
                            bundle_json: json.as_deref().unwrap_or(b"{}"),
                        },
                    );
                }
            }
            Err(error) => {
                stage::<()>(&run, "Bootstrap hooks", |_| Err(error));
            }
        }
    }
    if !run.cancelled() {
        match private::history_requested(ctx, &vars) {
            Ok(false) => {}
            request_history => {
                stage(&run, "Encrypted local history", |id| {
                    request_history?;
                    if !ready {
                        return Ok((
                            defer(
                                &run,
                                id,
                                "Complete private recovery before enrollment.",
                                RETRY,
                            ),
                            (),
                        ));
                    }
                    let Some(resolved) =
                        private::resolve_history(ctx, &request, &vars, &machine, false)?
                    else {
                        return Ok((
                            defer(
                                &run,
                                id,
                                "Enrollment requires a readable recovery identity.",
                                "Supply --identity or SETUP_AGE_IDENTITY and rerun dots bootstrap.",
                            ),
                            (),
                        ));
                    };
                    let recipients = bundle.as_ref().map_or(&[][..], |bundle| &bundle.recipients);
                    private::enroll(ctx, &run, id, &profile, &machine, resolved, recipients)
                        .map(|outcome| (outcome, ()))
                });
            }
        }
    }
    let status = print_summary(&run);
    Ok(run.end(status))
}

fn stage<T>(
    run: &Run<'_>,
    label: &str,
    action: impl FnOnce(StageId) -> Result<(Outcome, T), Fail>,
) -> Option<(Outcome, T)> {
    if run.cancelled() {
        return None;
    }
    let id = run.start(label);
    match action(id) {
        Ok(result) => {
            run.finish(id, result.0);
            Some(result)
        }
        Err(error) => {
            if !run.cancelled() {
                run.issue(id, Issue::new(error.to_string(), RETRY));
            }
            run.finish(id, Outcome::Failed);
            None
        }
    }
}

fn defer(run: &Run<'_>, id: StageId, message: &str, next: &str) -> Outcome {
    run.issue(id, Issue::new(message, next));
    Outcome::Deferred
}

fn print_summary(run: &Run<'_>) -> u8 {
    let mut counts = [0; 5];
    for stage in run.snapshot().stages {
        let (index, label) = match stage.outcome {
            Some(Outcome::Completed) => (0, "completed"),
            Some(Outcome::Deferred) => (1, "deferred"),
            Some(Outcome::Skipped) => (2, "skipped"),
            Some(Outcome::Failed) => (3, "failed"),
            Some(Outcome::Cancelled) => (4, "cancelled"),
            None => continue,
        };
        counts[index] += 1;
        if let Some(issue) = stage.issues.first() {
            println!(
                "{label}: {} — {} {}",
                stage.label, issue.diagnostic, issue.next
            );
        } else {
            println!("{label}: {}", stage.label);
        }
    }
    let [completed, deferred, skipped, failed, cancelled] = counts;
    let cancellation = if cancelled > 0 {
        format!(", {cancelled} cancelled")
    } else {
        String::new()
    };
    println!(
        "Bootstrap result: {completed} completed, {deferred} deferred, {skipped} skipped, {failed} failed{cancellation}."
    );
    u8::from(failed > 0)
}

pub fn remote(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let vars = Vars::read(&ctx.root)?;
    let host = args
        .get_one::<OsString>("HOST")
        .expect("required by CLI")
        .to_str()
        .ok_or_else(|| Fail::usage("the SSH host must be UTF-8"))?;
    if host.is_empty()
        || host.starts_with('-')
        || host.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Fail::usage("provide an SSH host for dots remote"));
    }
    let profile = args.get_one::<String>("profile").expect("required by CLI");
    if !vars.profiles.contains(profile) {
        return Err(Fail::usage(format!(
            "choose --profile from dots_profiles ({})",
            vars.profiles.join(", ")
        )));
    }
    let run = Run::begin(ctx, Sink::Report(ReportCommand::Remote));
    let from_origin = vars.source_repo.is_none();
    let url = match &vars.source_repo {
        None => source::origin(ctx, &run)?.ok_or_else(|| {
            Fail::usage("set dots_source_repo or configure a Git origin for dots remote")
        })?,
        Some(repo) => repo.transport_url().into_owned(),
    };
    let expected = crate::config::RepoSetting::parse(&url)?;
    source::validate_destination(ctx, &run, &url, &expected, from_origin)?;

    let mut native: Vec<OsString> = [
        "bootstrap",
        "remote",
        "--host",
        host,
        "--adopt",
        &url,
        "--remote-env",
        profile,
        "--install-mise",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    append_flags(
        &mut native,
        args,
        &["dry-run", "yes", "github-relay-read-only"],
    );
    append_strings(&mut native, args, "github-relay-repo");
    if let Some(path) = arg_path(ctx, args, "identity-file") {
        native.extend(["--identity-file".into(), path.into_os_string()]);
    }
    if let Some(port) = args.get_one::<u16>("port") {
        native.extend(["--port".into(), port.to_string().into()]);
    }
    if let Some(timeout) = args.get_one::<u64>("connect-timeout") {
        native.extend(["--connect-timeout".into(), timeout.to_string().into()]);
    }
    let command = Mise::new(ctx)?.command(native)?;
    let stage = run.start("Remote mise bootstrap");
    let status = match run.run(stage, command, Policy::Ordinary) {
        Ok(status) if status.success() => 0,
        Ok(_) => 1,
        Err(_) if run.cancelled() => 1,
        Err(error) => {
            eprintln!("dots: could not run mise for dots remote: {error}");
            1
        }
    };
    if status != 0 && !run.cancelled() {
        run.issue(
            stage,
            Issue::new(
                "native remote bootstrap failed",
                "Resolve the mise diagnostic and rerun dots remote.",
            ),
        );
    }
    run.finish(
        stage,
        if status == 0 {
            Outcome::Completed
        } else {
            Outcome::Failed
        },
    );
    Ok(run.end(status))
}
