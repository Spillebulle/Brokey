//! How an elevated `brokey-helper` is started and spoken to.
//!
//! The runner does not care how the privilege was obtained. It needs three
//! things: somewhere to write one JSON Plan and possibly a cancel line,
//! somewhere to read one JSON Event per line, and an exit code at the end.
//! Linux gets them from `pkexec`'s standard streams. Windows cannot:
//! `ShellExecuteEx` is the only call that elevates and it cannot redirect
//! standard streams, while `CreateProcess` can redirect them and cannot
//! elevate. So on Windows the two sides meet on a named pipe instead.
//!
//! This module is the only place in the workspace that obtains privilege.

use crate::transaction::runner::Line;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::Receiver;

mod unix;
use unix::Inner;

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
    unix::start(helper, wrapper)
}
