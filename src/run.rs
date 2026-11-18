use std::io;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command as ChildCommand, ExitStatus, Output, Stdio};
use std::sync::Mutex;

use crate::report::{self, Command, Report, Stage};
pub use crate::report::{Change, Issue, Outcome, StageId};
use crate::{Ctx, Fail};

pub enum Sink {
    Report(Command),
    Fragment(PathBuf),
    Unreported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Policy {
    Ordinary,
    Interactive,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub stages: Vec<Stage>,
    /// This is populated when the PTY supervisor owns a foreground stage.
    pub attachable: Option<StageId>,
}

pub struct Run<'a> {
    ctx: &'a Ctx,
    sink: Sink,
    stages: Mutex<Vec<Stage>>,
    rerun: Option<String>,
}

impl<'a> Run<'a> {
    pub fn ctx(&self) -> &'a Ctx {
        self.ctx
    }

    pub fn begin(ctx: &'a Ctx, sink: Sink) -> Self {
        Self {
            ctx,
            sink,
            stages: Mutex::new(Vec::new()),
            rerun: None,
        }
    }

    /// Record a safe rerun instruction with the required operands and effect flags.
    /// The caller supplies display text without credentials or private values.
    pub fn with_rerun(mut self, instruction: impl Into<String>) -> Self {
        self.rerun = Some(instruction.into());
        self
    }

    pub fn start(&self, label: impl Into<String>) -> StageId {
        self.add_stage(label.into(), false)
    }

    pub fn start_alongside(&self, label: impl Into<String>) -> StageId {
        self.add_stage(label.into(), true)
    }

    fn add_stage(&self, label: String, alongside: bool) -> StageId {
        let mut stages = self.stages.lock().expect("stage lock poisoned");
        let id = StageId(stages.len());
        stages.push(Stage {
            id,
            label,
            alongside,
            outcome: None,
            changes: Vec::new(),
            issues: Vec::new(),
        });
        self.update_fragment(&stages);
        id
    }

    /// Run one native child in the caller's foreground, with its stdio intact.
    /// Return the native status separately from spawn or wait errors.
    pub fn run(
        &self,
        id: StageId,
        mut cmd: ChildCommand,
        _policy: Policy,
    ) -> io::Result<ExitStatus> {
        if !self
            .stages
            .lock()
            .expect("stage lock poisoned")
            .get(id.0)
            .is_some_and(|stage| stage.outcome.is_none())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stage is not active",
            ));
        }
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
    }

    pub fn probe(&self, mut cmd: ChildCommand) -> io::Result<Output> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
    }

    pub fn change(&self, id: StageId, change: Change) {
        self.stages.lock().expect("stage lock poisoned")[id.0]
            .changes
            .push(change);
    }

    pub fn issue(&self, id: StageId, mut issue: Issue) {
        // The direct runner retains no raw child output or logs.
        issue.tail = None;
        issue.log = None;
        self.stages.lock().expect("stage lock poisoned")[id.0]
            .issues
            .push(issue);
    }

    pub fn finish(&self, id: StageId, outcome: Outcome) {
        let mut stages = self.stages.lock().expect("stage lock poisoned");
        stages[id.0].outcome = Some(outcome);
        self.update_fragment(&stages);
    }

    pub fn cancelled(&self) -> bool {
        false // Cancellation is owned by P-11.
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            stages: self.stages.lock().expect("stage lock poisoned").clone(),
            attachable: None,
        }
    }

    /// Plain mode has no attached view. This call never blocks the runner.
    pub fn view(&self, _stage: Option<StageId>) -> Result<(), Fail> {
        Err(Fail::unported("P-11", "opening a stage terminal view"))
    }

    /// Import callback metadata after its parent native stage returns.
    /// Reassign IDs so there are no collisions in the parent's snapshot.
    pub fn fold(&self, path: &Path) -> io::Result<()> {
        let fragment = report::load_fragment(path)?;
        let mut stages = self.stages.lock().expect("stage lock poisoned");
        for mut stage in fragment {
            stage.id = StageId(stages.len());
            stages.push(stage);
        }
        Ok(())
    }

    fn update_fragment(&self, stages: &[Stage]) {
        if let Sink::Fragment(path) = &self.sink
            && let Err(error) = report::save_fragment(path, stages)
        {
            eprintln!("dots: could not save stage metadata for dots bootstrap complete: {error}");
        }
    }

    pub fn end(self, status: u8) -> u8 {
        if let Sink::Report(command) = self.sink {
            let stages = self.stages.into_inner().expect("stage lock poisoned");
            let next =
                (status != 0 || stages.iter().any(|stage| !stage.issues.is_empty())).then(|| {
                    self.rerun.unwrap_or_else(|| {
                        format!(
                            "Rerun your original dots {} invocation with the same arguments.",
                            command.name()
                        )
                    })
                });
            let report = Report {
                command,
                finished_at_ms: report::timestamp(),
                status,
                cancelled: false,
                next,
                stages,
            };
            if let Err(error) = report::save(&self.ctx.env, &report) {
                eprintln!(
                    "dots: could not save the report for dots {}: {error}",
                    command.name()
                );
            }
        }
        status
    }
}

/// A forwarding handler returns the child's native exit code or its shell signal status.
pub fn exit_code(status: ExitStatus) -> u8 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)) as u8
}
