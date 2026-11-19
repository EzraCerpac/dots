use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output};

use nix::libc;

use crate::Fail;

use super::failure;

pub(super) const ARGS: &[&str] = &[
    "dir",
    "--no-banner",
    "--redact=100",
    "--exit-code=20",
    "--max-archive-depth=1",
    "--no-color",
    "--log-level=debug",
];

const RECORD_PREFIX: usize = 1024;
const SCAN_FAILED: &str = "gitleaks could not scan the outgoing commits; fix the scanner and retry";
const DEPTH_SKIP: &str = "gitleaks skipped an archive at the one-level scan limit; remove it from the outgoing commits and retry";
const FORMAT_SKIP: &str =
    "gitleaks skipped an unsupported file format; remove it from the outgoing commits and retry";

pub(super) fn screen(
    mut command: Command,
    probe: impl FnOnce(Command) -> Result<Output, Fail>,
) -> Result<(), Fail> {
    let reader = diagnostics_pipe(&mut command).map_err(|_| failure(SCAN_FAILED))?;
    std::thread::scope(|scope| {
        let read = scope.spawn(move || read_diagnostics(reader));
        let output = probe(command)?;
        let diagnostics = read
            .join()
            .ok()
            .and_then(Result::ok)
            .ok_or_else(|| failure(SCAN_FAILED))?;
        check(output.status.code(), diagnostics)
    })
}

fn diagnostics_pipe(command: &mut Command) -> io::Result<UnixStream> {
    let (reader, writer) = UnixStream::pair()?;
    let stderr = duplicate(&writer)?;
    let stdout = duplicate(&File::open("/dev/null")?)?;
    // Override the probe's capture pipes after the supervisor sets them up.
    // The probe still owns child lifecycle and cancellation.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(stderr.as_raw_fd(), libc::STDERR_FILENO) == -1
                || libc::dup2(stdout.as_raw_fd(), libc::STDOUT_FILENO) == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(reader)
}

fn duplicate(file: &impl AsRawFd) -> io::Result<OwnedFd> {
    // Keep our sources above stdio even if a caller closed a descriptor.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[derive(Default)]
struct Diagnostics {
    failed: bool,
    skipped: Option<&'static str>,
}

impl Diagnostics {
    fn record(&mut self, prefix: &[u8]) {
        if let Some((level, message)) = console_record(&String::from_utf8_lossy(prefix)) {
            self.failed |= matches!(level, "ERR" | "FTL" | "PNC");
            if let Some(reason) = coverage_skip(level, message) {
                self.skipped.get_or_insert(reason);
            }
        }
    }
}

fn read_diagnostics(stderr: impl Read) -> io::Result<Diagnostics> {
    let mut reader = BufReader::new(stderr);
    let mut diagnostics = Diagnostics::default();
    let mut prefix = [0; RECORD_PREFIX];
    let mut len = 0;
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            diagnostics.record(&prefix[..len]);
            return Ok(diagnostics);
        }
        let newline = bytes.iter().position(|&byte| byte == b'\n');
        let end = newline.unwrap_or(bytes.len());
        // Native messages and their scanner-generated fields precede the
        // variable-length path. Drain the tail as part of the same record.
        let keep = end.min(prefix.len() - len);
        prefix[len..len + keep].copy_from_slice(&bytes[..keep]);
        len += keep;
        if newline.is_some() {
            diagnostics.record(&prefix[..len]);
            len = 0;
            reader.consume(end + 1);
        } else {
            reader.consume(end);
        }
    }
}

fn check(code: Option<i32>, diagnostics: Diagnostics) -> Result<(), Fail> {
    match code {
        Some(0) => (),
        Some(20) => {
            return Err(failure(
                "gitleaks found potential secrets; remove them from every outgoing commit and retry",
            ));
        }
        _ => return Err(failure(SCAN_FAILED)),
    }
    if diagnostics.failed {
        return Err(failure(SCAN_FAILED));
    }
    diagnostics
        .skipped
        .map_or(Ok(()), |reason| Err(failure(reason)))
}

// Gitleaks v8 emits no-color Zerolog console records on stderr. Start at
// the message field so metadata and quoted paths cannot impersonate a skip.
fn console_record(line: &str) -> Option<(&str, &str)> {
    let (time, record) = line.split_once(' ')?;
    let time = time
        .strip_suffix("AM")
        .or_else(|| time.strip_suffix("PM"))?;
    let (hour, minute) = time.split_once(':')?;
    if !(1..=12).contains(&hour.parse::<u8>().ok()?)
        || minute.len() != 2
        || minute.parse::<u8>().ok()? >= 60
    {
        return None;
    }
    record.split_once(' ')
}

fn coverage_skip(level: &str, message: &str) -> Option<&'static str> {
    if level == "WRN" {
        if let Some(fields) =
            message.strip_prefix("skipping archive: exceeds max archive depth max_archive_depth=")
        {
            let (depth, path) = fields.split_once(" path=")?;
            if depth.parse::<u32>().is_ok() && !path.is_empty() {
                return Some(DEPTH_SKIP);
            }
        }
        if let Some(path) = message.strip_prefix("skipping unknown archive type path=")
            && !path.is_empty()
        {
            return Some(FORMAT_SKIP);
        }
    }
    if level == "DBG" {
        let fields = message.strip_prefix("skipping binary file mime_type=")?;
        let (mime, path) = fields.split_once(" path=")?;
        if mime.contains('/') && !mime.contains(char::is_whitespace) && !path.is_empty() {
            return Some(FORMAT_SKIP);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_stderr(code: Option<i32>, stderr: &[u8]) -> Result<(), Fail> {
        check(code, read_diagnostics(stderr).unwrap())
    }

    #[test]
    fn new_native_depth_limit_blocks_even_on_scanner_success() {
        let stderr = b"11:00PM WRN skipping archive: exceeds max archive depth max_archive_depth=1 path=outer.zip!inner.zip\n11:00PM INF no leaks found\n";
        assert_eq!(
            check_stderr(Some(0), stderr).unwrap_err().diagnostic,
            DEPTH_SKIP
        );
    }

    #[test]
    fn new_native_unsupported_formats_block_even_on_scanner_success() {
        for stderr in [
            "11:00PM DBG skipping binary file mime_type=application/pdf path=binary.dat\n",
            "11:00PM WRN skipping unknown archive type path=fixture.archive\n",
        ] {
            assert_eq!(
                check_stderr(Some(0), stderr.as_bytes())
                    .unwrap_err()
                    .diagnostic,
                FORMAT_SKIP
            );
        }
    }

    #[test]
    fn new_allowlist_and_lookalike_text_are_not_coverage_skips() {
        let stderr = concat!(
            "11:00PM DBG skipping file: global allowlist path=\"skipping binary file mime_type=application/pdf path=fixture\"\n",
            "11:00PM WRN unrelated message path=\"skipping archive: exceeds max archive depth max_archive_depth=1 path=inner.zip\"\n",
            "11:00PM INF skipped_archive=true note=\"skipping unknown archive type path=fixture\"\n",
            "skipping unknown archive type path=filename\n",
            "11:00PM WRN skipping archive: exceeds max archive depth\n",
            "11:00PM INF no leaks found\n",
        );
        assert!(check_stderr(Some(0), stderr.as_bytes()).is_ok());
        assert!(check_stderr(Some(0), b"").is_ok());
    }

    #[test]
    fn new_scanner_failures_and_findings_block() {
        for code in [None, Some(1), Some(13)] {
            assert_eq!(check_stderr(code, b"").unwrap_err().diagnostic, SCAN_FAILED);
        }
        let stderr = b"11:00PM ERR could not open archive inner file path=fixture.zip\n";
        assert_eq!(
            check_stderr(Some(0), stderr).unwrap_err().diagnostic,
            SCAN_FAILED
        );
        assert!(
            check_stderr(Some(20), b"")
                .unwrap_err()
                .diagnostic
                .contains("found potential secrets")
        );
    }

    #[test]
    fn new_scanner_drains_large_benign_output_and_ignores_lookalikes_in_a_long_path() {
        let benign = b"11:00PM DBG skipping file: global allowlist path=anything.pdf\n";
        let mut stderr = benign.repeat(32_768);
        stderr.extend_from_slice(b"11:00PM DBG skipping file: global allowlist path=\"");
        stderr.extend(std::iter::repeat_n(b'x', RECORD_PREFIX * 16));
        stderr.extend_from_slice(b" 11:00PM WRN skipping unknown archive type path=synthetic\"\n");
        assert!(check_stderr(Some(0), &stderr).is_ok());
    }

    #[test]
    fn new_scanner_detects_native_skip_after_large_output_with_a_long_path() {
        let benign = b"11:00PM DBG skipping file: global allowlist path=anything.pdf\n";
        let mut stderr = benign.repeat(32_768);
        stderr
            .extend_from_slice(b"11:00PM DBG skipping binary file mime_type=application/pdf path=");
        stderr.extend(std::iter::repeat_n(b'x', RECORD_PREFIX * 16));
        assert_eq!(
            check_stderr(Some(0), &stderr).unwrap_err().diagnostic,
            FORMAT_SKIP
        );
    }
}
