#![allow(dead_code)] // A suite can use the terminal without sending input.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd as _};
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::libc;

use crate::support::FixtureCommand;

const TIMEOUT: Duration = Duration::from_secs(25);

pub struct Pty {
    master: File,
    child: Child,
    transcript: Vec<u8>,
    cursor: usize,
    eof: bool,
}

impl Pty {
    pub fn spawn(mut command: FixtureCommand<'_>) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 25,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // openpty initializes both returned descriptors. Each is closed by its File.
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        };
        assert_eq!(result, 0, "openpty: {}", io::Error::last_os_error());
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        for descriptor in [&master, &slave] {
            assert_ne!(
                unsafe { libc::fcntl(descriptor.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
                -1,
                "set PTY close-on-exec: {}",
                io::Error::last_os_error()
            );
        }
        command
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // Establish a private process group and controlling terminal for /dev/tty users.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                #[cfg(target_os = "macos")]
                {
                    // Opening the controlling tty makes Darwin drain pending
                    // output on exit before closing the terminal.
                    let tty = libc::open(c"/dev/tty".as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                    if tty < 0 || libc::close(tty) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Self {
            master,
            child,
            transcript: Vec::new(),
            cursor: 0,
            eof: false,
        }
    }

    pub fn expect(&mut self, expected: &str) {
        let deadline = Instant::now() + TIMEOUT;
        let expected = expected.as_bytes();
        loop {
            if let Some(position) = self.transcript[self.cursor..]
                .windows(expected.len())
                .position(|window| window == expected)
            {
                self.cursor += position + expected.len();
                return;
            }
            assert!(
                !self.eof && self.read_until(deadline),
                "waiting for {:?} in terminal output:\n{}",
                String::from_utf8_lossy(expected),
                String::from_utf8_lossy(&self.transcript)
            );
        }
    }

    pub fn send(&mut self, input: &str) {
        self.master.write_all(input.as_bytes()).unwrap();
    }

    pub fn finish(mut self) -> (ExitStatus, String) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.eof {
            assert!(
                self.read_until(deadline),
                "terminal did not close:\n{}",
                String::from_utf8_lossy(&self.transcript)
            );
        }
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                let transcript = String::from_utf8_lossy(&self.transcript).replace("\r\n", "\n");
                return (status, transcript);
            }
            assert!(Instant::now() < deadline, "terminal child did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn read_until(&mut self, deadline: Instant) -> bool {
        let mut event = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let timeout = remaining.as_millis().max(1) as libc::c_int;
            match unsafe { libc::poll(&mut event, 1, timeout) } {
                0 => return false,
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => panic!("poll terminal: {}", io::Error::last_os_error()),
                _ => {}
            }

            let mut buffer = [0; 4096];
            match self.master.read(&mut buffer) {
                Ok(0) => self.eof = true,
                Ok(size) => self.transcript.extend_from_slice(&buffer[..size]),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => self.eof = true,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("read terminal: {error}"),
            }
            return true;
        }
        false
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if !self.eof || self.child.try_wait().ok().flatten().is_none() {
            // The child called setsid, so signal only the fixture's process group.
            unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}
