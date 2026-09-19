//! Plans and the runner: the privilege boundary of the whole application.
//!
//! - `plan` turns operations into steps, ordered so one helper run covers as
//!   many root steps as possible, with consecutive package-manager calls
//!   joined into one.
//! - `allow` is the closed list of what may run as root, Linux's shape of it
//!   (Windows will get its own in a later plan). It is a pure function of
//!   the plan; `brokey-helper` calls it and so does the runner's dry run.
//! - `progress` reads a tool's output for "(3/7) installing foo".
//! - `runner` executes a plan: root steps through `pkexec brokey-helper run`,
//!   session steps here, every line an event. The only `pkexec` call site.
//!   Linux only until a later plan gives Windows its own privilege path.
//!
//! `CancelToken` lives here rather than in `runner`: the page's bookkeeping
//! of a running plan (`crates/brokey/src/state.rs`) holds one per plan and
//! is itself no more Linux-specific than a plan id, so it stays built on
//! both platforms even while `runner`, the thing it cancels, is not.

#[cfg(unix)]
pub mod allow;
#[cfg(unix)]
pub mod elevate;
pub mod plan;
pub mod progress;
#[cfg(unix)]
pub mod runner;

#[cfg(unix)]
pub use allow::{Allowed, validate, validate_with};
pub use plan::{PARTIAL_UPGRADE_NOTICE, build, log_out_notice, notices, notices_in};
pub use progress::{ProgressParser, Reading};
#[cfg(unix)]
pub use runner::{Outcome, Runner, Sink};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared with whoever may stop a running plan. Cloning gives the same
/// token. Built on both platforms: it is a plain flag, not a process.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> CancelToken {
        CancelToken::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
