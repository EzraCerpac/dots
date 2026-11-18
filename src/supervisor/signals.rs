use nix::libc;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};

const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

static SEQUENCE: AtomicI32 = AtomicI32::new(0);
static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);
static INSTALLED: Mutex<Option<Installed>> = Mutex::new(None);

struct Installed {
    users: usize,
    previous: [libc::sigaction; SIGNALS.len()],
}

extern "C" fn notify(signal: libc::c_int) {
    LAST_SIGNAL.store(signal, Ordering::Relaxed);
    SEQUENCE.fetch_add(1, Ordering::Release);
}

pub(super) struct Signals {
    pub(super) start: libc::c_int,
}

impl Signals {
    pub(super) fn install() -> io::Result<Self> {
        let mut installed = INSTALLED.lock().unwrap();
        let start = SEQUENCE.load(Ordering::Acquire);
        if let Some(active) = installed.as_mut() {
            active.users += 1;
        } else {
            let mut previous = unsafe { std::mem::zeroed::<[libc::sigaction; SIGNALS.len()]>() };
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = notify as *const () as libc::sighandler_t;
            unsafe { libc::sigemptyset(&mut action.sa_mask) };
            for (index, signal) in SIGNALS.into_iter().enumerate() {
                if unsafe { libc::sigaction(signal, &action, &mut previous[index]) } == -1 {
                    let error = io::Error::last_os_error();
                    restore(&previous[..index]);
                    return Err(error);
                }
            }
            *installed = Some(Installed { users: 1, previous });
        }
        Ok(Self { start })
    }

    pub(super) fn pending(last: &mut libc::c_int) -> Option<libc::c_int> {
        let now = SEQUENCE.load(Ordering::Acquire);
        if *last == now {
            return None;
        }
        *last = now;
        Some(LAST_SIGNAL.load(Ordering::Relaxed))
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        let mut installed = INSTALLED.lock().unwrap();
        let active = installed.as_mut().unwrap();
        active.users -= 1;
        if active.users == 0 {
            restore(&active.previous);
            *installed = None;
        }
    }
}

fn restore(previous: &[libc::sigaction]) {
    for (signal, action) in SIGNALS.into_iter().zip(previous) {
        unsafe { libc::sigaction(signal, action, std::ptr::null_mut()) };
    }
}
