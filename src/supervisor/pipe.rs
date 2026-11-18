use nix::libc;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::process::{Child, Output};
use std::time::Duration;

use super::{Running, TICK};

pub(super) fn nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn ready(fd: RawFd, events: libc::c_short, wait: Duration) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let ms = wait.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    let result = unsafe { libc::poll(&mut poll, 1, ms) };
    if result == -1 {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::Interrupted => Ok(false),
            _ => Err(error),
        };
    }
    Ok(result != 0)
}

pub(super) struct Pipes {
    streams: [Stream; 2],
}

struct Stream {
    input: Option<File>,
    output: RawFd,
    pending: Vec<u8>,
    consumed: usize,
}

impl Stream {
    fn new(input: impl IntoRawFd, output: RawFd) -> io::Result<Self> {
        // This is a unique pipe end taken out of Child.
        let input = unsafe { File::from_raw_fd(input.into_raw_fd()) };
        nonblocking(input.as_raw_fd())?;
        Ok(Self {
            input: Some(input),
            output,
            pending: Vec::new(),
            consumed: 0,
        })
    }

    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let Some(input) = &mut self.input else {
            return Ok(0);
        };
        match input.read(bytes) {
            Ok(0) => {
                self.input = None;
                Ok(0)
            }
            Ok(count) => Ok(count),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(0)
            }
            Err(error) => Err(error),
        }
    }

    fn flush(&mut self) -> io::Result<bool> {
        let pending = &self.pending[self.consumed..];
        if pending.is_empty() || !ready(self.output, libc::POLLOUT, Duration::ZERO)? {
            return Ok(false);
        }
        // PIPE_BUF is at least 512 on supported Unix systems. Keep each write
        // within the minimum writable pipe unit so cancellation can be polled
        // while a redirected reader works through the previous output.
        let count =
            unsafe { libc::write(self.output, pending.as_ptr().cast(), pending.len().min(512)) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::BrokenPipe {
                self.pending.clear();
                self.consumed = 0;
                self.output = -1;
                return Ok(true);
            }
            return match error.kind() {
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => Ok(false),
                _ => Err(error),
            };
        }
        self.consumed += count as usize;
        if self.consumed == self.pending.len() {
            self.pending.clear();
            self.consumed = 0;
        }
        Ok(count != 0)
    }
}

impl Pipes {
    pub(super) fn take(child: &mut Child) -> io::Result<Self> {
        Ok(Self {
            streams: [
                Stream::new(child.stdout.take().unwrap(), libc::STDOUT_FILENO)?,
                Stream::new(child.stderr.take().unwrap(), libc::STDERR_FILENO)?,
            ],
        })
    }

    pub(super) fn collect(mut self, running: &mut Running<'_>) -> io::Result<Output> {
        let mut output = [Vec::new(), Vec::new()];
        let status = self.pump(running, false, |index, bytes| {
            output[index].extend_from_slice(bytes)
        })?;
        let [stdout, stderr] = output;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    pub(super) fn pump(
        &mut self,
        running: &mut Running<'_>,
        mut verbose: bool,
        mut record: impl FnMut(usize, &[u8]),
    ) -> io::Result<std::process::ExitStatus> {
        let mut bytes = [0; 8192];
        loop {
            let eof = self.streams.iter().all(|stream| stream.input.is_none());
            let pending_output = self.streams.iter().any(|stream| !stream.pending.is_empty());
            if let Some(status) = running.poll(eof && !pending_output, pending_output)? {
                return Ok(status);
            }
            if verbose && running.discard_display() {
                verbose = false;
                for stream in &mut self.streams {
                    stream.pending.clear();
                    stream.consumed = 0;
                }
            }
            let mut progressed = false;
            for (index, stream) in self.streams.iter_mut().enumerate() {
                if !stream.pending.is_empty() {
                    progressed |= stream.flush()?;
                }
                if stream.pending.is_empty() {
                    let count = stream.read(&mut bytes)?;
                    if count != 0 {
                        progressed = true;
                        record(index, &bytes[..count]);
                        if verbose && stream.output != -1 {
                            stream.pending.extend_from_slice(&bytes[..count]);
                        }
                    }
                }
            }
            if progressed {
                running.output_progress();
            } else {
                std::thread::sleep(TICK);
            }
        }
    }
}
