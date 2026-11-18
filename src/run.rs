use std::io::{self, Write};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command as ChildCommand, ExitStatus, Output, Stdio};
use std::sync::Mutex;

use crate::Ctx;
use crate::report::{self, Command, Report, Stage};
pub use crate::report::{Change, Issue, Outcome, StageId};
use crate::supervisor::{Mode, Supervisor};
use crate::ui::transcript::{write_report, write_stage_end, write_stage_start};

const CANCELLED_STAGE: StageId = StageId(usize::MAX);

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
}

pub struct Run<'a> {
    ctx: &'a Ctx,
    sink: Sink,
    stages: Mutex<Stages>,
    rerun: Option<String>,
    supervisor: Supervisor,
}

#[derive(Default)]
struct Stages {
    values: Vec<Stage>,
    finished_alongside: Vec<StageId>,
}

impl Stages {
    fn flush_alongside(&mut self, out: &mut impl Write, supervisor: &Supervisor) {
        for id in self.finished_alongside.drain(..) {
            let _ = supervisor.replay_alongside(id, out);
            let _ = write_stage_end(out, &self.values[id.0]);
        }
    }
}

impl<'a> Run<'a> {
    pub fn ctx(&self) -> &'a Ctx {
        self.ctx
    }

    pub fn begin(ctx: &'a Ctx, sink: Sink) -> Self {
        let mode = match &sink {
            Sink::Report(Command::Up) => Mode::up(ctx),
            Sink::Fragment(_) => Mode::Fragment,
            _ => Mode::Direct,
        };
        Self {
            ctx,
            sink,
            stages: Mutex::new(Stages::default()),
            rerun: None,
            supervisor: Supervisor::new(&ctx.env, mode),
        }
    }

    /// Record a safe rerun instruction with the required operands and effect flags.
    /// The caller supplies display text without credentials or private values.
    pub fn with_rerun(mut self, instruction: impl Into<String>) -> Self {
        self.rerun = Some(instruction.into());
        self
    }

    pub fn start(&self, label: impl Into<String>) -> StageId {
        self.add_stage(label.into(), false, None)
    }

    pub fn start_alongside(&self, label: impl Into<String>) -> StageId {
        self.add_stage(label.into(), true, None)
    }

    /// Record a known skip in one step, with a single result line during up.
    pub fn skip(&self, label: impl Into<String>) -> StageId {
        self.add_stage(label.into(), false, Some(Outcome::Skipped))
    }

    fn add_stage(&self, label: String, alongside: bool, outcome: Option<Outcome>) -> StageId {
        let mut stages = self.stages.lock().expect("stage lock poisoned");
        let Some(id) = self.supervisor.admit_stage(|| {
            let id = StageId(stages.values.len());
            stages.values.push(Stage {
                id,
                label,
                alongside,
                outcome,
                changes: Vec::new(),
                issues: Vec::new(),
            });
            id
        }) else {
            return CANCELLED_STAGE;
        };
        self.update_fragment(&stages.values);
        if self.up() && !alongside {
            let mut output = io::stdout().lock();
            stages.flush_alongside(&mut output, &self.supervisor);
            let stage = &stages.values[id.0];
            let _ = if outcome.is_some() {
                write_stage_end(&mut output, stage)
            } else {
                write_stage_start(&mut output, stage)
            };
        }
        id
    }

    /// Run a native child with this stage's foreground or alongside policy.
    /// Return the native status separately from spawn or wait errors.
    pub fn run(&self, id: StageId, cmd: ChildCommand, policy: Policy) -> io::Result<ExitStatus> {
        self.supervisor
            .run(id, self.stage_alongside(id)?, cmd, policy)
    }

    fn stage_alongside(&self, id: StageId) -> io::Result<bool> {
        if self.cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "dots run was cancelled",
            ));
        }
        self.stages
            .lock()
            .expect("stage lock poisoned")
            .values
            .get(id.0)
            .filter(|stage| stage.outcome.is_none())
            .map(|stage| stage.alongside)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "stage is not active"))
    }

    pub fn probe(&self, cmd: ChildCommand) -> io::Result<Output> {
        self.probe_with_stdin(cmd, Stdio::null())
    }

    /// Capture a probe, detaching its session when it belongs to an alongside stage.
    pub fn probe_for(&self, id: StageId, cmd: ChildCommand) -> io::Result<Output> {
        self.supervisor
            .probe(cmd, Stdio::null(), self.stage_alongside(id)?)
    }

    /// Capture a probe's output in memory with an explicit input channel.
    pub fn probe_with_stdin(&self, cmd: ChildCommand, input: Stdio) -> io::Result<Output> {
        self.supervisor.probe(cmd, input, false)
    }

    pub fn change(&self, id: StageId, change: Change) {
        if let Some(stage) = self
            .stages
            .lock()
            .expect("stage lock poisoned")
            .values
            .get_mut(id.0)
        {
            stage.changes.push(change);
        }
    }

    pub fn issue(&self, id: StageId, mut issue: Issue) {
        self.supervisor.enrich_issue(id, &mut issue);
        if let Some(stage) = self
            .stages
            .lock()
            .expect("stage lock poisoned")
            .values
            .get_mut(id.0)
        {
            stage.issues.push(issue);
        }
    }

    pub fn finish(&self, id: StageId, outcome: Outcome) {
        let mut stages = self.stages.lock().expect("stage lock poisoned");
        if let Some(stage) = stages.values.get_mut(id.0) {
            stage.outcome = Some(if self.cancelled() {
                Outcome::Cancelled
            } else {
                outcome
            });
            if self.up() {
                if stage.alongside {
                    stages.finished_alongside.push(id);
                } else {
                    let mut output = io::stdout().lock();
                    stages.flush_alongside(&mut output, &self.supervisor);
                    let _ = write_stage_end(&mut output, &stages.values[id.0]);
                }
            }
            self.update_fragment(&stages.values);
        }
    }

    pub fn cancelled(&self) -> bool {
        self.supervisor.cancelled()
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            stages: self
                .stages
                .lock()
                .expect("stage lock poisoned")
                .values
                .clone(),
        }
    }

    /// Import callback metadata after its parent native stage returns.
    /// Reassign IDs so there are no collisions in the parent's snapshot.
    pub fn fold(&self, path: &Path) -> io::Result<()> {
        let fragment = report::load_fragment(path)?;
        let mut stages = self.stages.lock().expect("stage lock poisoned");
        let cancelled = self.cancelled();
        for mut stage in fragment {
            stage.id = StageId(stages.values.len());
            if cancelled && stage.outcome.is_none() {
                stage.outcome = Some(Outcome::Cancelled);
            }
            stages.values.push(stage);
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

    fn up(&self) -> bool {
        matches!(self.sink, Sink::Report(Command::Up))
    }

    pub fn end(self, status: u8) -> u8 {
        let command = match self.sink {
            Sink::Report(command) => Some(command),
            _ => None,
        };
        let result = self.supervisor.complete(command, status);
        if result.cancelled {
            let mut stages = self.stages.lock().expect("stage lock poisoned");
            for stage in &mut stages.values {
                if stage.outcome.is_none() {
                    stage.outcome = Some(Outcome::Cancelled);
                }
            }
            self.update_fragment(&stages.values);
        }
        if let Sink::Report(command) = self.sink {
            let mut records = self.stages.into_inner().expect("stage lock poisoned");
            if command == Command::Up {
                records.flush_alongside(&mut io::stdout().lock(), &self.supervisor);
            }
            let mut stages = records.values;
            let rerun = self.rerun.unwrap_or_else(|| {
                format!(
                    "Rerun your original dots {} invocation with the same arguments.",
                    command.name()
                )
            });
            if result.cancelled && command == Command::Remote {
                let diagnostic = "The remote host's state is unknown after cancellation.";
                let next = "Inspect the remote host before retrying.";
                eprintln!("dots remote: {diagnostic} {next}");
                let stage = Stage {
                    id: StageId(stages.len()),
                    label: "Remote host".into(),
                    alongside: false,
                    outcome: Some(Outcome::Cancelled),
                    changes: Vec::new(),
                    issues: vec![Issue::new(diagnostic, next)],
                };
                stages.push(stage);
            }
            if command == Command::Up {
                for stage in &mut stages {
                    if stage.outcome == Some(Outcome::Failed) && stage.issues.is_empty() {
                        let mut issue = Issue::new("The stage failed.", &rerun);
                        self.supervisor.enrich_issue(stage.id, &mut issue);
                        stage.issues.push(issue);
                    }
                }
            }
            let next = (result.status != 0
                || result.cancelled
                || stages.iter().any(|stage| !stage.issues.is_empty()))
            .then_some(rerun);
            let mut report = Report {
                command,
                finished_at_ms: report::timestamp(),
                status: result.status,
                cancelled: result.cancelled,
                next,
                stages,
            };
            let log_id = self.supervisor.log_id();
            let saved = match report::save(&self.ctx.env, &report, log_id.as_deref()) {
                Ok(()) => {
                    self.supervisor.keep_logs();
                    if let Err(error) = report::prune(&self.ctx.env, command) {
                        eprintln!(
                            "dots: could not finish report retention for dots {}: {error}",
                            command.name()
                        );
                    }
                    true
                }
                Err(error) => {
                    eprintln!(
                        "dots: could not save the report for dots {}: {error}",
                        command.name()
                    );
                    for issue in report.stages.iter_mut().flat_map(|stage| &mut stage.issues) {
                        issue.log = None;
                    }
                    false
                }
            };
            if command == Command::Up {
                let _ = write_report(&mut io::stdout().lock(), &report, saved);
            }
        }
        result.status
    }
}

/// A forwarding handler returns the child's native exit code or its shell signal status.
pub fn exit_code(status: ExitStatus) -> u8 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)) as u8
}
