mod checkpoint;
mod keys;
mod native;
mod probe_state;
mod publication;
mod recipient_config;
mod recovery;
mod tracked;
mod watcher;

use std::ffi::OsString;
use std::path::Path;

use crate::config::{RepoSetting, Vars, selection};
use crate::report::Command;
use crate::run::{Issue, Outcome, Run, Sink, StageId};
use crate::{Args, Ctx, Fail};

use keys::LocalSettings;
use native::{Native, exists};

pub use checkpoint::{Checkpoint, CheckpointFailure, checkpoint, checkpoint_classified};

pub struct Restore<'a> {
    pub machine: &'a str,
    pub origin: &'a RepoSetting,
    pub identity: &'a Path,
    pub initialize: bool,
}

pub struct Enroll<'a> {
    pub profile: &'a str,
    pub machine: &'a str,
    pub origin: &'a RepoSetting,
    pub identity: &'a Path,
    pub recovery_identity: Option<&'a Path>,
    pub recipients: &'a [String],
}

pub fn backup(ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    let run = Run::begin(ctx, Sink::Report(Command::Backup));
    let stage = run.start("Back up settings history");
    let result = publish(&Native::new(&run, ctx));
    if result.is_ok() {
        println!("Settings backup published.");
    }
    report(
        run,
        stage,
        result,
        "resolve the issue, then rerun dots backup",
    )
}

fn publish(native: &Native<'_, '_>) -> Result<(), Fail> {
    let cmd = native.mise(&["settings", "get", "history.sync"])?;
    if native
        .text(cmd, "could not read the effective history sync mode")?
        .trim()
        != "manual"
    {
        return Err(invalid("history sync must be manual before backup"));
    }
    let ctx = native.ctx;
    let machine = selection::read(&ctx.root, &ctx.env)?
        .machine_id
        .ok_or_else(|| invalid("set a machine ID and matching host-ID history branch"))?;
    let branch = host(&machine)?;
    if LocalSettings::read(native)?.branch() != Some(&branch) {
        return Err(invalid(
            "config.local.toml must record the matching host-ID history branch",
        ));
    }
    let vars = Vars::read(&ctx.root)?;
    let expected = state_repo(&vars)?;
    let paths = tracked::capture_paths(native, Some(&branch))?;
    let (origin, effective_branch) = native.effective_origin()?;
    if effective_branch != branch {
        return Err(invalid(
            "the effective mise history branch must match host-ID",
        ));
    }
    let store = native.store()?;
    let _lock = native::lock(&store)?;
    checkpoint::require_no_pending(&store)?;
    native.require_no_pending_operations()?;
    let existing = exists(&store)?.then_some(store.as_path());
    if let Some(store) = existing {
        native.bare(store)?;
    }
    native.destinations(existing, &origin, expected, &vars)?;
    checkpoint::save(native, Some(&branch), &paths)?;
    native.bare(&store)?;
    let saved = recovery::revision(native, &store, recovery::MAIN_REF)?;
    native.require_origin(&origin, &branch)?;
    let transport = native.destinations(Some(&store), &origin, expected, &vars)?;
    tracked::capture_paths(native, Some(&branch))?.check_binding(paths.binding(), false)?;
    native.require_no_pending_operations()?;
    let probe = |args: &[&str], access: publication::GitAccess<'_>| {
        let mut git = native.git(Some(&store))?;
        if let Some(parameters) = publication::git_parameters(access, git.get_envs()) {
            git.env("GIT_CONFIG_PARAMETERS", parameters);
        }
        git.envs(publication::git_environment(access)).args(args);
        let diagnostic = "could not verify the history publication range";
        if access.is_local() {
            native.probe(git, diagnostic)
        } else {
            native
                .run
                .probe_detached(git, std::time::Duration::from_secs(30))
                .map_err(|_| blocked(diagnostic))
        }
    };
    let policy = native.mise(&["settings", "get", "history.allow_plaintext_history"])?;
    let http = if publication::allows_plaintext(native.probe(
        policy,
        "could not read mise's effective history.allow_plaintext_history setting",
    )?)? {
        publication::http_policy(&probe, &transport)?
    } else {
        publication::preflight(
            probe,
            &branch,
            &origin,
            &saved,
            publication::has_ssh_launcher(|name| ctx.env.get(name)),
        )?
    };
    let mut cmd = native.mise(&["bootstrap", "dotfiles", "sync"])?;
    recovery::unmodified_git_graph(&mut cmd)
        .env_remove("__MISE_HISTORY_LOCAL")
        .env("GIT_TERMINAL_PROMPT", "0");
    publication::constrain_http(&mut cmd, http.as_ref());
    native.output(
        cmd,
        "native history sync failed; the settings checkpoint was kept locally and publication could not be verified. Check the private history remote and use mise dot status",
    )?;
    let pending = || {
        blocked(
            "settings checkpoint saved locally; publication is pending. Run mise dot status to inspect incoming changes or conflicts before retrying dots backup",
        )
    };
    let unconfirmed = || {
        blocked(
            "settings checkpoint saved locally; publication could not be verified. Check the private history remote",
        )
    };
    let readback_transport = native
        .destinations(Some(&store), &origin, expected, &vars)
        .map_err(|_| unconfirmed())?;
    if readback_transport != transport
        && (publication::is_http(&transport) || publication::is_http(&readback_transport))
    {
        return Err(unconfirmed());
    }
    let reference = format!("refs/heads/{branch}");
    let remote = remote_checkpoint(native, &store, &origin, &reference, http.as_ref())
        .map_err(|_| unconfirmed())?
        .ok_or_else(pending)?;
    if !recovery::ancestor(native, &store, &saved, &remote).map_err(|_| unconfirmed())? {
        return Err(pending());
    }
    Ok(())
}

fn remote_checkpoint(
    native: &Native<'_, '_>,
    store: &Path,
    origin: &str,
    reference: &str,
    http: Option<&publication::HttpPolicy>,
) -> Result<Option<String>, Fail> {
    let diagnostic = "could not independently verify the private history branch";
    let mut cmd = native.git(Some(store))?;
    cmd.args([
        "ls-remote",
        "--exit-code",
        "--refs",
        "--",
        origin,
        reference,
    ]);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    publication::constrain_http(&mut cmd, http);
    let output = native.probe(cmd, diagnostic)?;
    if output.status.code() == Some(2) && output.stdout.is_empty() && output.stderr.is_empty() {
        return Ok(None);
    }
    let (oid, actual) = std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|text| text.trim_end_matches('\n').split_once('\t'))
        .ok_or_else(|| blocked(diagnostic))?;
    if !output.status.success() || actual != reference || !recovery::valid_oid(oid) {
        return Err(blocked(diagnostic));
    }
    Ok(Some(oid.to_owned()))
}

pub fn restore_command(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let mut run = Run::begin(ctx, Sink::Report(Command::Restore));
    let initialize = args.get_flag("initialize-history");
    if initialize {
        run = run.with_rerun("Rerun dots restore --initialize-history after resolving the issue.");
    }
    let stage = run.start("Restore private settings history");
    let native = Native::new(&run, ctx);
    let result = (|| {
        watcher::require_stopped(&native)?;
        let selected = selection::read(&ctx.root, &ctx.env)?;
        let machine = selected
            .machine_id
            .as_deref()
            .ok_or_else(|| invalid("set a machine ID to restore its history"))?;
        let local = LocalSettings::read(&native)?;
        let origin = local
            .env("SETUP_HISTORY_ORIGIN")
            .or_else(|| local.origin())
            .ok_or_else(|| invalid("record a private history origin in config.local.toml"))?;
        let origin = RepoSetting::parse(origin)?;
        let fallback = native.home()?.join(".config/age/keys.txt");
        let identity = local
            .env("SETUP_AGE_IDENTITY")
            .map_or(fallback.as_path(), Path::new);
        recovery::restore(
            &native,
            Restore {
                machine,
                origin: &origin,
                identity,
                initialize,
            },
        )
    })();
    if result.is_ok() {
        println!("Private settings restored.");
    }
    report(
        run,
        stage,
        result,
        "resolve the issue, then rerun dots restore",
    )
}

pub fn restore(run: &Run<'_>, stage: StageId, settings: Restore<'_>) -> Outcome {
    let native = Native::new(run, run.ctx());
    let result =
        watcher::require_stopped(&native).and_then(|()| recovery::restore(&native, settings));
    stage_outcome(
        run,
        stage,
        &result,
        "resolve the issue, then rerun dots restore",
    )
}

pub fn enroll_command(ctx: &Ctx, args: &Args) -> Result<u8, Fail> {
    let run = Run::begin(ctx, Sink::Unreported);
    let native = Native::new(&run, ctx);
    let vars = Vars::read(&ctx.root)?;
    let state = state_repo(&vars)?;
    let local = LocalSettings::read(&native)?;
    let supplied = args
        .get_one::<String>("origin")
        .map(String::as_str)
        .or_else(|| local.env("SETUP_HISTORY_ORIGIN"))
        .or_else(|| local.origin());
    let origin = supplied.map(RepoSetting::parse).transpose()?;
    let recipients = args
        .get_many::<String>("recipient")
        .into_iter()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let settings = Enroll {
        profile: args.get_one::<String>("profile").expect("required profile"),
        machine: args
            .get_one::<String>("machine-id")
            .expect("required machine ID"),
        identity: Path::new(
            args.get_one::<OsString>("identity")
                .expect("required identity"),
        ),
        origin: origin.as_ref().unwrap_or(state),
        recovery_identity: None,
        recipients: &recipients,
    };
    prepare_enrollment(native, &vars, settings, true)?;
    println!("Prepared encrypted settings history.");
    Ok(run.end(0))
}

pub fn enroll(run: &Run<'_>, stage: StageId, settings: Enroll<'_>) -> Outcome {
    let ctx = run.ctx();
    let result = Vars::read(&ctx.root)
        .and_then(|vars| prepare_enrollment(Native::new(run, ctx), &vars, settings, false));
    stage_outcome(
        run,
        stage,
        &result,
        "enroll after configuring encrypted host files and a separate recovery recipient",
    )
}

fn prepare_enrollment(
    mut native: Native<'_, '_>,
    vars: &Vars,
    settings: Enroll<'_>,
    persist_selection: bool,
) -> Result<(), Fail> {
    let Enroll {
        profile,
        machine,
        origin,
        identity,
        recovery_identity,
        recipients,
    } = settings;
    if !vars.profiles.iter().any(|configured| configured == profile) {
        return Err(invalid(
            "enrollment profile must be listed in dots_profiles",
        ));
    }
    let ctx = native.ctx;
    let branch = host(machine)?;
    validate_origin(&native, vars, origin)?;
    if persist_selection {
        let selected = selection::read(&ctx.root, &ctx.env)?;
        native.select(selection::Selection {
            profile: Some(profile.to_owned()),
            machine_id: Some(machine.to_owned()),
            selectors: selected.selectors,
        });
    }

    let (identity, own_recipients) = keys::read_identity(&native, identity)?;
    let mut local = LocalSettings::read(&native)?;
    let mut all = local.recipients()?;
    all.extend(recipients.iter().cloned());
    let has_recovery = all.iter().any(|key| !own_recipients.contains(key));
    all.extend(own_recipients);
    keys::check_recipients(&native, &all)?;
    if !has_recovery {
        return Err(Fail::new(
            3,
            "history enrollment needs a distinct recovery recipient; use --recipient, or supply recovery identity with --identity or SETUP_AGE_IDENTITY to dots bootstrap",
        ));
    }
    let paths = tracked::paths(&native, Some(&branch))?;
    if paths.is_empty() {
        return Err(Fail::new(
            3,
            format!(
                "no tracked history files for {branch}; declare encrypted track entries with this host variant"
            ),
        ));
    }
    paths.secure(&native)?;
    if persist_selection {
        selection::persist(
            &ctx.root,
            &ctx.env,
            selection::Persist {
                profile,
                machine_id: machine,
                origin: Some(&origin.transport_url()),
            },
        )?;
        local = LocalSettings::read(&native)?;
    }
    // Standalone enrollment selects the key for later restore. Bootstrap
    // supplies a separate recovery key and preserves an existing selection.
    local.set_recovery_identity(recovery_identity.unwrap_or(&identity), persist_selection)?;
    local.set_history(&branch, origin, &identity, &all)?;
    local.install(&native)
}

fn host(machine: &str) -> Result<String, Fail> {
    let mut chars = machine.bytes();
    let alnum = |c: u8| c.is_ascii_digit() || c.is_ascii_lowercase();
    if !chars.next().is_some_and(alnum) || !chars.all(|c| alnum(c) || c == b'-') {
        return Err(invalid("invalid machine ID for settings history"));
    }
    Ok(format!("host-{machine}"))
}

fn state_repo(vars: &Vars) -> Result<&RepoSetting, Fail> {
    vars.state_repo
        .as_ref()
        .ok_or_else(|| invalid("configure a private dots_state_repo in config.toml"))
}

fn validate_origin(native: &Native<'_, '_>, vars: &Vars, origin: &RepoSetting) -> Result<(), Fail> {
    let expected = state_repo(vars)?;
    if !expected.matches(&origin.transport_url()) {
        return Err(invalid("history origin must match private dots_state_repo"));
    }
    let local = LocalSettings::read(native)?;
    for recorded in [local.origin(), local.env("SETUP_HISTORY_ORIGIN")]
        .into_iter()
        .flatten()
    {
        if !expected.matches(recorded) {
            return Err(invalid(
                "recorded history origin in config.local.toml must match private dots_state_repo",
            ));
        }
    }
    Ok(())
}

fn report(run: Run<'_>, stage: StageId, result: Result<(), Fail>, next: &str) -> Result<u8, Fail> {
    let outcome = stage_outcome(&run, stage, &result, next);
    run.finish(stage, outcome);
    let status = run.end(result.as_ref().err().map_or(0, |error| error.status));
    result.map(|()| status).map_err(|mut error| {
        error.status = status;
        error
    })
}

fn stage_outcome(run: &Run<'_>, stage: StageId, result: &Result<(), Fail>, next: &str) -> Outcome {
    match result {
        Ok(()) => Outcome::Completed,
        Err(error) => {
            run.issue(stage, Issue::new(error.to_string(), next));
            if error.status == 3 {
                Outcome::Deferred
            } else {
                Outcome::Failed
            }
        }
    }
}

fn invalid(diagnostic: impl Into<String>) -> Fail {
    Fail::usage(diagnostic)
}

fn blocked(diagnostic: impl Into<String>) -> Fail {
    Fail::new(1, diagnostic)
}
