//! What machine this is, on Windows. Task 2 fills this in.

#[cfg(windows)]
use crate::model::SystemInfo;

#[cfg(windows)]
pub fn detect() -> SystemInfo {
    SystemInfo {
        distro_id: "windows".to_string(),
        distro_like: Vec::new(),
        pretty_name: "Windows".to_string(),
        arch: std::env::consts::ARCH.to_string(),
        desktop: None,
        session: None,
    }
}
