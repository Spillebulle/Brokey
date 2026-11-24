//! The machine, as the scan is allowed to see it: a read-only view.
//!
//! Everything the scan asks about files, the registry, services, scheduled
//! tasks and firewall rules goes through [`SystemView`]. That is what lets the
//! scan be tested against a JSON snapshot of a machine ([`FixtureView`]) and
//! lets it be proved to change nothing: there is no method here that writes.
//! The trait and its types compile on every OS; `LiveView` is Windows only.

use crate::sources::windows::arp;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod fixture;
#[cfg(windows)]
pub mod windows;

pub use fixture::FixtureView;
#[cfg(windows)]
pub use windows::LiveView;

/// What kind of reparse point a path is, as far as the scan cares.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Reparse {
    /// An ordinary file or folder.
    #[default]
    None,
    /// A junction, a symbolic link or a mount point: a name that leads
    /// somewhere else, and so a way to make a clean-up reach outside itself.
    NameSurrogate,
    /// A cloud-synced placeholder, whose content lives elsewhere and which
    /// reading may download.
    Cloud,
}

/// One thing in a folder.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    #[serde(default)]
    pub is_dir: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub reparse: Reparse,
}

/// Where in the registry. The 32-bit view is its own word so that the scan
/// can say which one it found a key in.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hive {
    Hklm,
    Hklm32,
    Hkcu,
}

impl From<arp::Hive> for Hive {
    fn from(h: arp::Hive) -> Hive {
        match h {
            arp::Hive::Machine => Hive::Hklm,
            arp::Hive::Machine32 => Hive::Hklm32,
            arp::Hive::User => Hive::Hkcu,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    pub image_path: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ScheduledTask {
    /// The task's path under the Task Scheduler root, such as `\Vendor\Update`.
    pub path: String,
    /// The `<Exec><Command>` of each action.
    pub actions: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FirewallRule {
    pub name: String,
    /// The `App=` field: the program the rule is about.
    pub program: String,
}

/// The well-known places a leftover can be in.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Known {
    ProgramFiles,
    ProgramFilesX86,
    ProgramData,
    AppData,
    LocalAppData,
    LocalLow,
    Documents,
    SavedGames,
    StartMenuMachine,
    StartMenuUser,
    Desktop,
    Windows,
    Profile,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ViewError {
    /// It is there and may not be read. The scan counts these and says so.
    #[error("{0} could not be read.")]
    Unreadable(String),
    /// It is not there.
    #[error("{0} does not exist.")]
    Missing(String),
}

pub trait SystemView {
    fn known_folder(&self, which: Known) -> Option<PathBuf>;
    fn list(&self, dir: &Path) -> Result<Vec<Entry>, ViewError>;
    fn exists(&self, path: &Path) -> bool;
    /// Of the path itself, not of what it leads to.
    fn reparse(&self, path: &Path) -> Reparse;
    fn dir_size(&self, dir: &Path) -> u64;
    fn subkeys(&self, hive: Hive, path: &str) -> Result<Vec<String>, ViewError>;
    /// Each value's data as text.
    fn values(&self, hive: Hive, path: &str) -> Result<Vec<(String, String)>, ViewError>;
    fn shortcut_target(&self, lnk: &Path) -> Option<PathBuf>;
    fn services(&self) -> Vec<Service>;
    fn tasks(&self) -> Vec<ScheduledTask>;
    fn firewall_rules(&self) -> Vec<FirewallRule>;
}
