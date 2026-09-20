//! The setup executable: Brokey's own binary with an MSI on the end of it.
//!
//! Run normally this is just Brokey. Run with `--install` it lifts the MSI
//! back out of its own file and installs it through a window of Brokey's own,
//! so somebody installing Brokey for the first time sees Brokey rather than
//! Windows Installer.

pub mod payload;
