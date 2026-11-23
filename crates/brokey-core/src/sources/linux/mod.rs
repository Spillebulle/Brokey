//! The sources a Linux machine has. Built only on Linux; `sources::all`
//! selects between this module and `windows`.

pub mod alpmdb;
pub mod apt;
pub mod aur;
pub mod chwd;
pub mod dnf;
pub mod flatpak;
pub mod fwupd;
pub mod pacman;
pub mod snap;

/// GitHub releases are shared with Windows and live in `sources::github`;
/// the old path still names them, which is how the AUR source reaches them.
pub use super::github;
