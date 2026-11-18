use std::path::PathBuf;

use crate::Fail;
use crate::config::{Root, Vars};
use crate::run::{Outcome, Run};

pub enum Dir {
    Update,
    Bootstrap,
}

pub struct Hook {
    pub path: PathBuf,
    pub label: String,
}

pub enum HookEnv<'a> {
    Update {
        tools_ok: bool,
        tools_upgraded: bool,
    },
    Bootstrap {
        private_ready: bool,
        restored: bool,
        bundle_json: &'a [u8],
    },
}

pub fn list(_root: &Root, _dir: Dir, _vars: &Vars, _profile: &str) -> Result<Vec<Hook>, Fail> {
    Err(Fail::unported("P-02", "listing hooks"))
}

pub fn run(run: &Run<'_>, hook: &Hook, _env: HookEnv<'_>) -> Outcome {
    let stage = run.start(&hook.label);
    run.issue(
        stage,
        crate::run::Issue::new(
            Fail::unported("P-02", "running hooks").diagnostic,
            "rerun with a build that includes P-02",
        ),
    );
    run.finish(stage, Outcome::Failed);
    Outcome::Failed
}
