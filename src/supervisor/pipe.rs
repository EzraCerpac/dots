use nix::libc;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{Running, Supervisor, TICK, terminal};

const OUTPUT_WRITER: &str = "__dots_supervisor_output_writer";
const OUTPUT_WRITER_ENV: &str = "DOTS_INTERNAL_OUTPUT_WRITER";
static REEXEC_AVAILABLE: AtomicBool = AtomicBool::new(cfg!(test));
// Requests fit the minimum POSIX PIPE_BUF with their two-byte length.
const PACKET: usize = 512;
const PAYLOAD: usize = PACKET - 2;
const CANCEL_DISPLAY_GRACE: Duration = Duration::from_millis(100);

pub(super) fn nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn ready(fd: RawFd, events: libc::c_short, wait: Duration) -> io::Result<bool> {
    poll(&mut [event(fd, events)], wait)
}

fn event(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

fn poll(fds: &mut [libc::pollfd], wait: Duration) -> io::Result<bool> {
    let ms = wait.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
    let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
    if result == -1 {
        let error = io::Error::last_os_error();
        return match error.kind() {
            io::ErrorKind::Interrupted => Ok(false),
            _ => Err(error),
        };
    }
    Ok(result != 0)
}

fn write_output(fd: RawFd, tty: bool, bytes: &[u8]) -> io::Result<usize> {
    let write = || {
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    };
    if tty {
        // A captured child can hold the terminal during a display write.
        terminal::without_sigttou(write)
    } else {
        write()
    }
}

fn write_result(result: io::Result<usize>) -> io::Result<Option<usize>> {
    match result {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(None)
        }
        result => result.map(Some),
    }
}

/// Register re-exec support at the executable entry point, or run the private
/// writer. Stdout is its display and stdin/stderr carry requests and replies.
pub fn output_writer(mut args: impl Iterator<Item = OsString>) -> Option<u8> {
    if args.nth(1).as_deref() == Some(OsStr::new(OUTPUT_WRITER))
        && args.next().is_none()
        && std::env::var_os(OUTPUT_WRITER_ENV).as_deref() == Some(OsStr::new("1"))
    {
        return Some(u8::from(write_requests(libc::STDOUT_FILENO).is_err()));
    }
    REEXEC_AVAILABLE.store(true, Ordering::Relaxed);
    None
}

fn write_requests(fd: RawFd) -> io::Result<()> {
    let (send, receive) = std::sync::mpsc::sync_channel::<([u8; PAYLOAD], usize)>(1);
    std::thread::Builder::new().spawn(move || {
        let mut replies = io::stderr().lock();
        for (bytes, count) in receive {
            let response = match write_output(fd, false, &bytes[..count]) {
                Ok(count) => count as i32,
                Err(error) => -error.raw_os_error().unwrap_or(libc::EIO),
            };
            if replies.write_all(&response.to_ne_bytes()).is_err() {
                break;
            }
        }
    })?;
    // EOF still ends the helper if the supervisor vanishes during a write.
    // The caller sends no subsequent request until it receives the reply.
    let mut requests = io::stdin().lock();
    loop {
        let mut size = [0; 2];
        if requests.read(&mut size[..1])? == 0 {
            return Ok(());
        }
        requests.read_exact(&mut size[1..])?;
        let count = usize::from(u16::from_ne_bytes(size));
        let mut bytes = [0; PAYLOAD];
        let chunk = bytes
            .get_mut(..count)
            .ok_or_else(|| io::Error::other("invalid display request"))?;
        requests.read_exact(chunk)?;
        send.send((bytes, count))
            .map_err(|_| io::Error::other("display writer stopped"))?;
    }
}

struct Writer {
    child: Child,
    requests: ChildStdin,
    replies: ChildStderr,
    pending: bool,
}

impl Writer {
    fn new(ctx: &crate::Ctx, fd: RawFd) -> io::Result<Self> {
        // Only the private request and reply pipes change status flags.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if duplicate == -1 {
            return Err(io::Error::last_os_error());
        }
        let output = unsafe { File::from_raw_fd(duplicate) };
        let executable = if cfg!(test) {
            std::env::current_exe()?
        } else {
            ctx.pinned_executable()?
        };
        let mut command = Command::new(executable);
        command
            .env_clear()
            .envs(&ctx.env.vars)
            .process_group(0)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped());
        configure_writer(&mut command, output);
        let mut child = command.spawn()?;
        let writer = Self {
            requests: child.stdin.take().unwrap(),
            replies: child.stderr.take().unwrap(),
            child,
            pending: false,
        };
        nonblocking(writer.requests.as_raw_fd())?;
        nonblocking(writer.replies.as_raw_fd())?;
        Ok(writer)
    }

    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let count = bytes.len().min(PAYLOAD);
        let mut packet = [0; PACKET];
        packet[..2].copy_from_slice(&(count as u16).to_ne_bytes());
        packet[2..2 + count].copy_from_slice(&bytes[..count]);
        self.requests.write_all(&packet[..2 + count])?;
        self.pending = true;
        Ok(())
    }

    fn receive(&mut self) -> io::Result<Option<usize>> {
        let mut response = [0; 4];
        match self.replies.read(&mut response) {
            Ok(4) => {
                self.pending = false;
                let value = i32::from_ne_bytes(response);
                write_result(if value < 0 {
                    Err(io::Error::from_raw_os_error(-value))
                } else {
                    Ok(value as usize)
                })
            }
            Ok(_) => Err(io::Error::other("display writer closed its reply pipe")),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(not(test))]
fn configure_writer(command: &mut Command, output: File) {
    command
        .arg(OUTPUT_WRITER)
        .env(OUTPUT_WRITER_ENV, "1")
        .stdout(output);
}

// The libtest launcher itself prints on stdout. Keep that stream off the
// fixture pipe while the child exercises the real writer protocol.
#[cfg(test)]
const FIXTURE_OUTPUT: RawFd = 97;
#[cfg(test)]
const FIXTURE_ROLE: &str = "DOTS_PRIVATE_TEST_OUTPUT_WRITER";

#[cfg(test)]
fn configure_writer(command: &mut Command, output: File) {
    command
        .args([
            "--exact",
            "supervisor::pipe::output_writer_fixture",
            "--nocapture",
        ])
        .env(FIXTURE_ROLE, "1")
        .stdout(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(output.as_raw_fd(), FIXTURE_OUTPUT) == -1
                || libc::fcntl(FIXTURE_OUTPUT, libc::F_SETFD, 0) == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
#[test]
fn output_writer_fixture() {
    if std::env::var_os(FIXTURE_ROLE).is_some() {
        std::process::exit(i32::from(write_requests(FIXTURE_OUTPUT).is_err()));
    }
}

struct Display {
    fd: RawFd,
    tty: bool,
    pipe: bool,
    writer: Option<Writer>,
}

impl Display {
    fn new(fd: RawFd) -> Self {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let pipe = REEXEC_AVAILABLE.load(Ordering::Relaxed)
            && unsafe { libc::fstat(fd, &mut stat) } == 0
            && stat.st_mode & libc::S_IFMT == libc::S_IFIFO;
        Self {
            fd,
            tty: unsafe { libc::isatty(fd) } != 0,
            pipe,
            writer: None,
        }
    }

    fn pending(&self) -> Option<&Writer> {
        self.writer.as_ref().filter(|writer| writer.pending)
    }

    fn event(&self) -> libc::pollfd {
        self.pending().map_or_else(
            || event(self.fd, libc::POLLOUT),
            |writer| event(writer.replies.as_raw_fd(), libc::POLLIN),
        )
    }

    fn write(&mut self, ctx: &crate::Ctx, bytes: &[u8]) -> io::Result<Option<usize>> {
        if let Some(writer) = self.writer.as_mut().filter(|writer| writer.pending) {
            return writer.receive();
        }
        if !ready(self.fd, libc::POLLOUT, Duration::ZERO)? {
            return Ok(None);
        }
        #[cfg(test)]
        AFTER_WRITABLE.with(|hook| {
            if let Some(f) = hook.take() {
                f();
            }
        });
        if !self.pipe {
            return write_result(write_output(
                self.fd,
                self.tty,
                &bytes[..bytes.len().min(PACKET)],
            ));
        }
        // POLLOUT does not reserve capacity on a pipe shared with another
        // writer. Keep the supervisor available to cancel and retire stages.
        if self.writer.is_none() {
            self.writer = Some(Writer::new(ctx, self.fd)?);
        }
        let writer = self.writer.as_mut().unwrap();
        writer.send(bytes)?;
        writer.receive()
    }
}

#[cfg(test)]
thread_local! {
    static AFTER_WRITABLE: std::cell::Cell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
#[test]
fn console_cancels_when_another_writer_fills_the_pipe_after_poll() {
    use std::sync::mpsc;

    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path();
    let env = crate::Env {
        vars: [
            "DOTS_ROOT",
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "MISE_CONFIG_DIR",
            "MISE_SYSTEM_CONFIG_DIR",
            "MISE_DATA_DIR",
            "MISE_SYSTEM_DATA_DIR",
            "MISE_STATE_DIR",
            "MISE_CACHE_DIR",
            "MISE_TMP_DIR",
            "MISE_GLOBAL_CONFIG_FILE",
            "MISE_INSTALLS_DIR",
            "MISE_INSTALL_STORE_DIR",
            "MISE_DOWNLOADS_DIR",
            "MISE_PLUGINS_DIR",
            "MISE_SHIMS_DIR",
            "MISE_SYSTEM_INSTALLS_DIR",
            "MISE_SYSTEM_SHIMS_DIR",
            "MISE_SYSTEM_PLUGINS_DIRS",
            "TMPDIR",
            "TMP",
            "TEMP",
        ]
        .into_iter()
        .map(|key| (key.into(), path.as_os_str().into()))
        .collect(),
        cwd: path.to_path_buf(),
    };
    let ctx = crate::Ctx {
        root: crate::config::Root::resolve(&env).unwrap(),
        env,
        flags: crate::Flags::default(),
    };
    let supervisor = Supervisor::new(&ctx, super::Mode::Direct);
    let (timely, after_release) = std::thread::scope(|scope| {
        let (reader, mut writer) = io::pipe().unwrap();
        let fd = writer.as_raw_fd();
        let original = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(original >= 0 && original & libc::O_NONBLOCK == 0);
        let (writable, polled) = mpsc::channel();
        let (resume, released) = mpsc::channel();
        let (done, result) = mpsc::channel();
        let supervisor = &supervisor;
        let writing = scope.spawn(move || {
            AFTER_WRITABLE.with(|slot| {
                slot.set(Some(Box::new(move || {
                    writable.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(2)).unwrap();
                })));
            });
            done.send(
                Console::new(supervisor, fd)
                    .write(b"x")
                    .map_err(|error| error.kind()),
            )
            .unwrap();
        });
        polled.recv_timeout(Duration::from_secs(2)).unwrap();
        // Only the private fixture pipe is toggled, while its writer is parked.
        nonblocking(fd).unwrap();
        for chunk in [&[0; 512][..], &[0][..]] {
            loop {
                match writer.write_all(chunk) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("fill fixture pipe: {error}"),
                }
            }
        }
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, original) }, 0);
        let restored = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(restored >= 0 && restored & libc::O_NONBLOCK == 0);
        supervisor.state().cancel(Some(libc::SIGINT), None);
        resume.send(()).unwrap();
        let timely = result.recv_timeout(Duration::from_millis(350));
        // Close the private reader before joining, even if the write blocked.
        drop(reader);
        writing.join().unwrap();
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFL) }, restored);
        (timely, result.try_recv().ok())
    });
    assert_eq!(
        timely,
        Ok(Err(io::ErrorKind::WouldBlock)),
        "write could only finish after the reader closed: {after_release:?}"
    );
}

/// The foreground runner owns these writes. A blocked reader never prevents
/// processing cancellation or finishing the report.
pub(crate) struct Console<'a> {
    supervisor: &'a Supervisor<'a>,
    display: Display,
}

impl<'a> Console<'a> {
    pub(super) fn new(supervisor: &'a Supervisor<'a>, fd: RawFd) -> Self {
        Self {
            supervisor,
            display: Display::new(fd),
        }
    }
}

impl Write for Console<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut cancelled_at = None;
        loop {
            if let Some(written) = self.display.write(self.supervisor.ctx, bytes)? {
                return Ok(written);
            }
            if self.supervisor.cancelled() {
                let since = *cancelled_at.get_or_insert_with(Instant::now);
                if self.display.pending().is_none() || since.elapsed() >= CANCEL_DISPLAY_GRACE {
                    self.display.writer = None;
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "output blocked during cancellation",
                    ));
                }
            }
            poll(&mut [self.display.event()], TICK)?;
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) struct Pipes {
    streams: [Stream; 2],
}

struct Stream {
    input: Option<File>,
    display: Display,
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
            display: Display::new(output),
            pending: Vec::new(),
            consumed: 0,
        })
    }

    fn read(&mut self, bytes: &mut [u8]) -> io::Result<Option<usize>> {
        let Some(input) = &mut self.input else {
            return Ok(None);
        };
        match input.read(bytes) {
            Ok(0) => {
                self.input = None;
                Ok(Some(0))
            }
            Ok(count) => Ok(Some(count)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn flush(&mut self, ctx: &crate::Ctx) -> io::Result<bool> {
        let pending = &self.pending[self.consumed..];
        if pending.is_empty() {
            return Ok(false);
        }
        let count = match self.display.write(ctx, pending) {
            Ok(Some(count)) => count,
            Ok(None) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                self.discard_display();
                self.display.fd = -1;
                return Ok(true);
            }
            Err(error) => return Err(error),
        };
        self.consumed += count;
        if self.consumed == self.pending.len() {
            self.pending.clear();
            self.consumed = 0;
        }
        Ok(count != 0)
    }

    fn discard_display(&mut self) {
        self.display.writer = None;
        self.pending.clear();
        self.consumed = 0;
    }

    fn event(&self) -> libc::pollfd {
        if self.pending.is_empty() {
            event(
                self.input.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                libc::POLLIN,
            )
        } else {
            self.display.event()
        }
    }
}

impl Pipes {
    pub(super) fn drained(&self) -> bool {
        self.streams.iter().all(|stream| stream.input.is_none())
    }

    pub(super) fn take(child: &mut Child) -> io::Result<Self> {
        Ok(Self {
            streams: [
                Stream::new(child.stdout.take().unwrap(), libc::STDOUT_FILENO)?,
                Stream::new(child.stderr.take().unwrap(), libc::STDERR_FILENO)?,
            ],
        })
    }

    pub(super) fn collect(self, running: &mut Running<'_>) -> io::Result<Output> {
        self.collect_limited(running, usize::MAX)
    }

    pub(super) fn collect_limited(
        mut self,
        running: &mut Running<'_>,
        limit: usize,
    ) -> io::Result<Output> {
        let mut output = [Vec::new(), Vec::new()];
        let mut remaining = limit;
        let status = self.pump(running, false, |index, bytes| {
            remaining = remaining.checked_sub(bytes.len()).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "probe output exceeded its capture limit",
                )
            })?;
            output[index].extend_from_slice(bytes);
            Ok(())
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
        mut record: impl FnMut(usize, &[u8]) -> io::Result<()>,
    ) -> io::Result<std::process::ExitStatus> {
        let mut bytes = [0; 8192];
        let mut capture_error = None;
        loop {
            let eof = self.drained();
            let pending_output = self.streams.iter().any(|stream| !stream.pending.is_empty());
            match running.poll(eof, pending_output) {
                Ok(Some(status)) => return capture_error.map_or(Ok(status), Err),
                Ok(None) => {}
                Err(error) => return Err(capture_error.unwrap_or(error)),
            }
            if verbose && running.discard_display() {
                verbose = false;
                for stream in &mut self.streams {
                    stream.discard_display();
                }
            }
            let mut progressed = false;
            for (index, stream) in self.streams.iter_mut().enumerate() {
                if !stream.pending.is_empty() {
                    progressed |= stream.flush(running.supervisor.ctx)?;
                }
                if stream.pending.is_empty()
                    && let Some(count) = stream.read(&mut bytes)?
                {
                    // A new EOF can complete the probe as surely as new bytes.
                    progressed = true;
                    if count != 0 {
                        if !running.discard_capture()
                            && let Err(error) = record(index, &bytes[..count])
                        {
                            if !running.stop_capture() {
                                return Err(error);
                            }
                            capture_error = Some(error);
                        }
                        if verbose && stream.display.fd != -1 {
                            stream.pending.extend_from_slice(&bytes[..count]);
                        }
                    }
                }
            }
            if progressed {
                running.output_progress();
            } else {
                poll(
                    &mut [self.streams[0].event(), self.streams[1].event()],
                    TICK,
                )?;
            }
        }
    }
}

#[cfg(test)]
#[test]
fn a_new_eof_counts_as_progress_once() {
    let (read, writer) = io::pipe().unwrap();
    let mut stream = Stream::new(read, libc::STDOUT_FILENO).unwrap();
    let mut bytes = [0; 1];
    assert_eq!(stream.read(&mut bytes).unwrap(), None);
    drop(writer);
    assert_eq!(stream.read(&mut bytes).unwrap(), Some(0));
    assert_eq!(stream.read(&mut bytes).unwrap(), None);
}
