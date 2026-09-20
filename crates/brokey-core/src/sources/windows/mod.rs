//! The sources a Windows machine has. Not gated to Windows: `arp`'s filter
//! is a pure function of a checked-in fixture and is tested on both
//! platforms, the way `system::windows` already is. `sources::all` selects
//! between this module and `linux` at runtime.

pub mod arp;
pub mod icon;
pub mod pe;
pub mod winget;
