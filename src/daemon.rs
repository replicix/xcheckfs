//! Going to the background the way libfuse's `fuse_daemonize` does: fork
//! before any thread exists, `setsid`, `chdir /`, and keep the parent
//! waiting until the child reports that the mount succeeded (or why it
//! failed), so the exit status of `xcheckfs mount --background` is
//! meaningful.

use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};

/// Write end of the status pipe, held by the daemon until the mount is up.
pub struct Notifier {
    pipe: Option<std::fs::File>,
}

impl Notifier {
    /// Tells the waiting parent that the mount is up and detaches stdio.
    pub fn ready(&mut self) {
        if let Some(mut p) = self.pipe.take() {
            let _ = p.write_all(b"OK");
            redirect_stdio();
        }
    }

    /// Tells the waiting parent why the daemon is giving up.
    pub fn failed(&mut self, msg: &str) {
        if let Some(mut p) = self.pipe.take() {
            let _ = p.write_all(format!("ERR{msg}").as_bytes());
        }
    }
}

/// Forks. Returns in the child only; the parent exits with the child's
/// mount result. Must be called while the process is single-threaded.
pub fn daemonize() -> anyhow::Result<Notifier> {
    let mut fds = [0; 2];
    // SAFETY: valid array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        anyhow::bail!("pipe: {}", std::io::Error::last_os_error());
    }
    // SAFETY: fresh descriptors.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // SAFETY: single-threaded at this point (documented precondition).
    match unsafe { libc::fork() } {
        -1 => anyhow::bail!("fork: {}", std::io::Error::last_os_error()),
        0 => {
            drop(r);
            // SAFETY: plain syscalls in the child.
            unsafe {
                libc::setsid();
                libc::chdir(c"/".as_ptr());
            }
            Ok(Notifier { pipe: Some(std::fs::File::from(w)) })
        }
        _child => {
            drop(w);
            let mut msg = String::new();
            let _ = std::fs::File::from(r).read_to_string(&mut msg);
            if msg == "OK" {
                std::process::exit(0);
            }
            let msg = msg.strip_prefix("ERR").unwrap_or("daemon exited before mounting");
            eprintln!("xcheckfs: {msg}");
            std::process::exit(1);
        }
    }
}

fn redirect_stdio() {
    // SAFETY: plain syscalls on standard descriptors.
    unsafe {
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if null >= 0 {
            libc::dup2(null, 0);
            libc::dup2(null, 1);
            libc::dup2(null, 2);
            if null > 2 {
                libc::close(null);
            }
        }
    }
}
