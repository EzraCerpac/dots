//! Plain scrollback for update stage boundaries and finished reports.

use std::fmt;
use std::io::{self, Write};

use crate::report::{Change, Outcome, Report, Stage};

use super::reports::{attention_disposition, display_path, needs_attention, needs_terminal_escape};

/// Flush the boundary before the caller gives a native child the foreground.
pub fn write_stage_start(out: &mut impl Write, stage: &Stage) -> io::Result<()> {
    writeln!(out, "==> {}", stage_label(stage))?;
    out.flush()
}

/// Call from the foreground, including when announcing a finished alongside stage.
pub fn write_stage_end(out: &mut impl Write, stage: &Stage) -> io::Result<()> {
    let outcome = match stage.outcome {
        Some(Outcome::Completed) => "completed",
        Some(Outcome::Deferred) => "deferred",
        Some(Outcome::Skipped) => "skipped",
        Some(Outcome::Failed) => "failed",
        Some(Outcome::Cancelled) => "cancelled",
        None => "unfinished",
    };
    writeln!(out, "{}: {outcome}", stage_label(stage))?;
    out.flush()
}

/// Write the same report at the end of a run and for a saved replay.
/// Set `saved` after a successful save or when replaying a saved report.
pub fn write_report(out: &mut impl Write, report: &Report, saved: bool) -> io::Result<()> {
    let attention = needs_attention(report);
    let result = match (report.cancelled, report.status) {
        (true, _) => "Cancelled",
        (false, 0) if attention => "Completed with issues",
        (false, 0) => "Complete",
        _ => "Failed",
    };
    writeln!(
        out,
        "dots {}: {result} (exit {})",
        report.command.name(),
        report.status,
    )?;

    for stage in &report.stages {
        for change in &stage.changes {
            if stage.label == "dots manager" && change.item == "dots" {
                let before = terminal_text(&change.before);
                let after = terminal_text(&change.after);
                writeln!(out, "dots {before} -> {after}")?;
            } else {
                write_change(out, change)?;
            }
        }
    }

    if attention {
        writeln!(out, "Needs attention")?;
        for stage in &report.stages {
            if stage.issues.is_empty()
                && let Some(disposition) = attention_disposition(stage)
            {
                writeln!(out, "  {} [{disposition}]", stage_label(stage))?;
            }
            for issue in &stage.issues {
                writeln!(
                    out,
                    "  {}: {}",
                    stage_label(stage),
                    terminal_text(&issue.diagnostic),
                )?;
                if let Some(status) = issue.terminal_exit {
                    writeln!(out, "    Exit status: {status}")?;
                }
                writeln!(out, "    Next: {}", terminal_text(&issue.next))?;
                if issue.terminal_exit.is_some() {
                    writeln!(out, "    Output was shown on the terminal")?;
                    continue;
                }
                if let Some(tail) = &issue.tail {
                    for line in tail.lines() {
                        writeln!(out, "    | {}", terminal_text(line))?;
                    }
                }
                if let Some(log) = &issue.log {
                    writeln!(out, "    Log: {}", display_path(log))?;
                }
            }
        }
    }
    if let Some(next) = &report.next {
        writeln!(out, "Rerun: {}", terminal_text(next))?;
    }
    if saved {
        writeln!(out, "Details and logs · dots up --last")?;
    }
    out.flush()
}

fn write_change(out: &mut impl Write, change: &Change) -> io::Result<()> {
    let missing = |value: &str| matches!(value, "" | "not installed" | "not present");
    let item = terminal_text(&change.item);
    let before = terminal_text(&change.before);
    let after = terminal_text(&change.after);
    match (missing(&change.before), missing(&change.after)) {
        (true, false) => writeln!(out, "    installed  {item}  {after}"),
        (false, true) => writeln!(out, "    removed  {item}  {before}"),
        (false, false) if change.before != change.after => {
            writeln!(out, "    updated  {item}  {before} → {after}")
        }
        _ if change.before.is_empty() && change.after.is_empty() => {
            writeln!(out, "    changed  {item}")
        }
        _ => writeln!(out, "    changed  {item}  {before} → {after}"),
    }
}

fn stage_label(stage: &Stage) -> impl fmt::Display {
    let alongside = if stage.alongside { " (alongside)" } else { "" };
    let label = terminal_text(&stage.label);
    fmt::from_fn(move |f| write!(f, "{label}{alongside}"))
}

fn terminal_text(value: &str) -> impl fmt::Display {
    fmt::from_fn(move |f| {
        use fmt::Write as _;

        for ch in value.chars() {
            if needs_terminal_escape(ch) {
                write!(f, "{}", ch.escape_default())?;
            } else {
                f.write_char(ch)?;
            }
        }
        Ok(())
    })
}
