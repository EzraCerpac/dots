use std::io::{self, PipeReader, PipeWriter, Write};
use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nix::libc;

use crate::Fail;

/// One private stream per hook. Both ends are close-on-exec in the parent.
pub struct BundlePipe {
    reader: PipeReader,
    stop: Arc<AtomicBool>,
    writer: Option<JoinHandle<()>>,
}

impl BundlePipe {
    pub fn new(json: &[u8]) -> Result<Self, Fail> {
        let (reader, writer) = io::pipe().map_err(pipe_error)?;
        let fd = writer.as_raw_fd();
        // SAFETY: the pipe owns this valid descriptor for both calls.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
                return Err(pipe_error(io::Error::last_os_error()));
            }
        }

        let json = json.to_vec();
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let writer = thread::Builder::new()
            .spawn(move || write_pipe(writer, &json, &cancelled))
            .map_err(pipe_error)?;
        Ok(Self {
            reader,
            stop,
            writer: Some(writer),
        })
    }

    pub fn attach(&self, command: &mut Command) -> Result<(), Fail> {
        let reader = self.reader.try_clone().map_err(pipe_error)?;
        command.env("DOTS_BUNDLE_FD", reader.as_raw_fd().to_string());
        // SAFETY: the closure owns the read end and changes only its child's
        // descriptor flags. fcntl is async-signal-safe.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(reader.as_raw_fd(), libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(())
    }

    pub(super) fn stdin(&self) -> Result<Stdio, Fail> {
        self.reader.try_clone().map(Stdio::from).map_err(pipe_error)
    }
}

impl Drop for BundlePipe {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.take() {
            self.stop.store(true, Ordering::Relaxed);
            writer.thread().unpark();
            let _ = writer.join();
        }
    }
}

fn write_pipe(mut writer: PipeWriter, mut bytes: &[u8], stop: &AtomicBool) {
    while !bytes.is_empty() && !stop.load(Ordering::Relaxed) {
        match writer.write(bytes) {
            Ok(0) => break,
            Ok(n) => bytes = &bytes[n..],
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::park_timeout(Duration::from_millis(2));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn pipe_error(error: io::Error) -> Fail {
    Fail::new(
        1,
        format!("could not prepare the private bootstrap input stream: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_payload_reaches_only_the_attached_child_and_leaves_stdin_intact() {
        let payload = vec![b'x'; 512 * 1024];
        let pipe = BundlePipe::new(&payload).unwrap();
        let mut hook = Command::new("/bin/sh");
        hook.args(["-c", "cat /dev/fd/\"$DOTS_BUNDLE_FD\"; cat"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        pipe.attach(&mut hook).unwrap();
        let fd = hook
            .get_envs()
            .find(|(key, _)| *key == "DOTS_BUNDLE_FD")
            .unwrap()
            .1
            .unwrap();
        let ordinary = Command::new("/bin/sh")
            .args([
                "-c",
                "test -z \"${DOTS_BUNDLE_FD+x}\" && test ! -e /dev/fd/\"$1\"",
                "sh",
            ])
            .arg(fd)
            .env_remove("DOTS_BUNDLE_FD")
            .status()
            .unwrap();
        assert!(ordinary.success());
        let mut child = hook.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"prompt input")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(&output.stdout[..payload.len()], payload);
        assert_eq!(&output.stdout[payload.len()..], b"prompt input");
    }

    #[test]
    fn dropping_a_full_pipe_does_not_wait_for_a_hook() {
        let pipe = BundlePipe::new(&vec![b'x'; 1024 * 1024]).unwrap();
        let mut unspawned = Command::new("/does-not-exist");
        pipe.attach(&mut unspawned).unwrap();
        assert!(unspawned.spawn().is_err());
        drop(pipe);
    }
}
