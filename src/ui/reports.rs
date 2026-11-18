use std::fmt::Write as _;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use nix::libc;

use crate::report::{self, Command, Outcome, Report, Stage};
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
        writeln!(
            out,
            "dots {}  {}  {} (exit {})\n  {}",
            report.command.name(),
            finished_at(report.finished_at_ms),
            outcome(&report),
            report.status,
            display_path(&path),
        )
        .map_err(|error| Fail::new(1, format!("could not write saved reports: {error}")))?;
    }
    Ok(0)
}

pub(super) fn display_path(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    let mut display = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for ch in chunk.valid().chars() {
            if ch == '\\' || needs_terminal_escape(ch) {
                display.extend(ch.escape_default());
            } else {
                display.push(ch);
            }
        }
        for byte in chunk.invalid() {
            write!(display, "\\x{byte:02x}").unwrap();
        }
    }
    display
}

pub(super) fn needs_terminal_escape(ch: char) -> bool {
    // Line separators and bidi controls can change the presentation of adjacent text.
    ch.is_control()
        || matches!(ch, '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

pub(super) fn attention_disposition(stage: &Stage) -> Option<&'static str> {
    match stage.outcome {
        Some(Outcome::Deferred) => Some("deferred"),
        Some(Outcome::Failed) => Some("failed"),
        _ => None,
    }
}

pub(super) fn needs_attention(report: &Report) -> bool {
    report
        .stages
        .iter()
        .any(|stage| !stage.issues.is_empty() || attention_disposition(stage).is_some())
}

fn outcome(report: &Report) -> &'static str {
    if report.cancelled {
        return "cancelled";
    }
    match report.status {
        0 if needs_attention(report) => "needs attention",
        0 => "completed",
        _ => "failed",
    }
}

fn finished_at(milliseconds: u64) -> String {
    let fallback = || format!("{milliseconds} ms since Unix epoch");
    let Ok(seconds) = (milliseconds / 1000).try_into() else {
        return fallback();
    };
    let mut utc = MaybeUninit::<libc::tm>::uninit();
    // gmtime_r initializes the caller's buffer on success.
    let Some(utc) = (unsafe { libc::gmtime_r(&seconds, utc.as_mut_ptr()).as_ref() }) else {
        return fallback();
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    use super::display_path;

    #[test]
    fn report_paths_distinguish_raw_bytes_from_escapes() {
        for (bytes, expected) in [
            (
                "/reports/café's file.json".as_bytes(),
                "/reports/café's file.json",
            ),
            (b"/reports/line\nnext.json", r"/reports/line\nnext.json"),
            (br"/reports/line\nnext.json", r"/reports/line\\nnext.json"),
            (b"/reports/\xff.json", r"/reports/\xff.json"),
            (br"/reports/\xff.json", r"/reports/\\xff.json"),
            ("/reports/�.json".as_bytes(), "/reports/�.json"),
            (b"/reports/\x1b.json", r"/reports/\u{1b}.json"),
        ] {
            assert_eq!(display_path(Path::new(OsStr::from_bytes(bytes))), expected);
        }
    }
}
