use crate::config::{Exceptions, selection};
use crate::run::{Outcome, Run, Sink};
use crate::{Args, Ctx, Fail};

use super::{ChildEnv, run_stage};

#[derive(Clone, Copy)]
pub enum Mode {
    Update,
    InstallOnly,
}

pub fn install(ctx: &Ctx, _args: &Args) -> Result<u8, Fail> {
    let exceptions = Exceptions::read(&ctx.root)?;
    let run = Run::begin(ctx, Sink::Unreported);
    let failed = run_selected(&run, &exceptions, Mode::InstallOnly)?;
    Ok(run.end(u8::from(failed)))
}

pub fn run(run: &Run<'_>, exceptions: &Exceptions, mode: Mode) -> Result<(), Fail> {
    run_selected(run, exceptions, mode).map(|_| ())
}

fn run_selected(run: &Run<'_>, exceptions: &Exceptions, mode: Mode) -> Result<bool, Fail> {
    let ctx = run.ctx();
    let selected = selection::read(&ctx.root, &ctx.env)?;
    let profile = selected.profile.as_deref().unwrap_or_default();
    let mut failed = false;
    for recipe in exceptions.selected(profile)? {
        if run.cancelled() {
            break;
        }
        let label = format!("Installer exception: {}", recipe.name);
        if matches!(mode, Mode::InstallOnly) {
            println!("{label}");
        }
        let args = if matches!(mode, Mode::InstallOnly) || recipe.install_only {
            &["--install-only"][..]
        } else {
            &[]
        };
        let outcome = run_stage(run, &label, &recipe.path, args, ChildEnv::Recipe(mode));
        if matches!(mode, Mode::InstallOnly) && outcome == Outcome::Failed && !run.cancelled() {
            eprintln!("Failed installer exception: {}", recipe.name);
        }
        failed |= outcome == Outcome::Failed;
    }
    Ok(failed)
}
