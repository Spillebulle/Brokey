//! How an elevated `brokey-helper` is started and spoken to.
//!
//! The runner does not care how the privilege was obtained. It needs three
//! things: somewhere to write one JSON Plan and possibly a cancel line,
//! somewhere to read one JSON Event per line, and an exit code at the end.
//! Linux gets them from `pkexec`'s standard streams. Windows cannot:
//! `ShellExecuteEx` is the only call that elevates and it cannot redirect
//! standard streams, while `CreateProcess` can redirect them and cannot
//! elevate. So on Windows the two sides meet on named pipes instead: one
//! per direction, as `pkexec`'s stdin and stdout are one per direction,
//! because a Windows handle serialises every operation on its file object
//! and one duplex pipe would let a pending read hold up the plan's write.
//!
//! This module is the only place in the workspace that obtains privilege.

use crate::transaction::runner::Line;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::Receiver;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix::Inner;

/// The Windows half of the privilege seam: a named pipe with its own DACL,
/// and `start`, which elevates the helper with `ShellExecuteEx`'s `runas`
/// verb and meets it on that pipe.
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::Inner;
/// The two pipe names, derived from the one base name the helper is given
/// on its command line. Re-exported because `brokey-helper` derives the
/// same two names from the same base, and one definition is the only way
/// the two sides cannot disagree about which pipe is which.
#[cfg(windows)]
pub use windows::{events_pipe_name, plan_pipe_name};

/// Neither Unix nor Windows: there is nothing to wait on, only the sentence
/// `start` already returned as an `Err`. This keeps `Elevated` one type on
/// every platform this builds for.
#[cfg(not(any(unix, windows)))]
struct Inner;

#[cfg(not(any(unix, windows)))]
impl Inner {
    fn wait(&mut self) -> std::io::Result<Option<i32>> {
        Err(std::io::Error::other(
            "Brokey cannot obtain Administrator on this system.",
        ))
    }
}

/// One elevated helper run, however the platform started it.
pub struct Elevated {
    /// The plan goes down this. Taken by the writer thread, which owns it
    /// so that the far end sees a close when that thread ends.
    pub input: Option<Box<dyn Write + Send>>,
    /// Whatever the helper wrote, one line at a time.
    pub lines: Receiver<Line>,
    inner: Inner,
}

impl Elevated {
    /// Wait for the helper to finish. `None` when the platform reported no
    /// code, which is not by itself a failure.
    ///
    /// There is no `kill` beside this on purpose. A running helper is
    /// stopped by writing `CANCEL_LINE` to `input`, which lets it finish the
    /// step it is on; killing a package manager part-way through is worse
    /// than waiting for it, and on Linux the child belongs to root and could
    /// not be killed from here anyway.
    pub fn wait(&mut self) -> std::io::Result<Option<i32>> {
        self.inner.wait()
    }
}

/// Start `helper` with privilege. `wrapper` is the program and any leading
/// arguments that obtain it, `pkexec` on Linux; it is ignored on a platform
/// that has its own way.
pub fn start(helper: &Path, wrapper: &[String]) -> std::io::Result<Elevated> {
    #[cfg(unix)]
    {
        unix::start(helper, wrapper)
    }
    #[cfg(windows)]
    {
        windows::start(helper, wrapper)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (helper, wrapper);
        Err(std::io::Error::other(
            "Brokey cannot obtain Administrator on this system.",
        ))
    }
}
