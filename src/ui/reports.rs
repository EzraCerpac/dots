use std::io::{self, Write};
use std::mem::MaybeUninit;

use nix::libc;

use crate::report::{self, Command, Outcome, Report};
use crate::{Env, Fail};

pub(super) fn list(env: &Env, command: Option<Command>, out: &mut impl Write) -> Result<u8, Fail> {
    let entries = report::list(env, command)
        .map_err(|error| Fail::new(1, format!("could not list saved reports: {error}")))?;
    for entry in entries {
        let report = match report::load(env, &entry.id) {
            Ok(report) => report,
            // Retention can remove an entry between listing and loading it.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(Fail::new(
                    1,
                    format!("could not read saved report: {error}"),
                ));
            }
        };
        let path = report::path(env, &entry.id)
            .map_err(|error| Fail::new(1, format!("could not locate saved report: {error}")))?;
        let path: String = path
            .to_string_lossy()
            .chars()
            .flat_map(char::escape_debug)
            .collect();
        writeln!(
            out,
            "dots {}  {}  {} (exit {})\n  {path}",
            report.command.name(),
            finished_at(report.finished_at_ms),
            outcome(&report),
            report.status,
        )
        .map_err(|error| Fail::new(1, format!("could not write saved reports: {error}")))?;
    }
    Ok(0)
}

fn outcome(report: &Report) -> &'static str {
    match report.status {
        130 => "cancelled",
        0 if report.stages.iter().any(|stage| {
            !stage.issues.is_empty()
                || matches!(stage.outcome, Some(Outcome::Failed | Outcome::Deferred))
        }) =>
        {
            "needs attention"
        }
        0 => "completed",
        _ => "failed",
    }
}

fn finished_at(milliseconds: u64) -> String {
    let seconds = (milliseconds / 1000) as libc::time_t;
    let mut utc = MaybeUninit::<libc::tm>::uninit();
    // gmtime_r initializes the caller's buffer on success.
    let Some(utc) = (unsafe { libc::gmtime_r(&seconds, utc.as_mut_ptr()).as_ref() }) else {
        return format!("{milliseconds} ms since Unix epoch");
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} UTC",
        i64::from(utc.tm_year) + 1900,
        utc.tm_mon + 1,
        utc.tm_mday,
        utc.tm_hour,
        utc.tm_min,
        utc.tm_sec,
        milliseconds % 1000,
    )
}
