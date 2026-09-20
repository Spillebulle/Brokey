//! Plans and the runner: the privilege boundary of the whole application.
//!
//! - `plan` turns operations into steps, ordered so one helper run covers as
//!   many root steps as possible, with consecutive package-manager calls
//!   joined into one.
//! - `allow` is the closed list of what may run as root, built on both
//!   platforms. It is a pure function of the plan; `brokey-helper` calls it
//!   and so does the runner's dry run.
//! - `progress` reads a tool's output for "(3/7) installing foo".
//! - `runner` executes a plan: root steps go to `brokey-helper`, however
//!   `elevate` starts it on this platform; session steps run here, every
//!   line an event.
//!
//! `CancelToken` lives here rather than in `runner`: the page's bookkeeping
//! of a running plan (`crates/brokey/src/state.rs`) holds one per plan and
//! is itself no more platform-specific than a plan id, so it stays built on
//! both platforms alongside `runner`, the thing it cancels.

pub mod allow;
pub mod elevate;
pub mod plan;
pub mod progress;
pub mod runner;

pub use allow::{Allowed, validate, validate_with};
pub use plan::{PARTIAL_UPGRADE_NOTICE, build, log_out_notice, notices, notices_in};
pub use progress::{ProgressParser, Reading};
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
