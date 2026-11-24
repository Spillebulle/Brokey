//! Leftover cleanup after an uninstall: the scan, the guard that decides
//! what may be touched, and the holding area that makes clearing reversible.
//! The helper is what moves things; everything here either reads the machine
//! or is a pure function.

pub mod guard;
pub mod hints;
pub mod holding;
pub mod internal;
pub mod scan;
pub mod steam;
pub mod view;
