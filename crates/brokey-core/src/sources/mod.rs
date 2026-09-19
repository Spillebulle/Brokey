//! One module per source. [`all`] builds them in interface order; each one
//! reports its own availability, so an unavailable source stays in the list
//! and the page can say why.
//!
//! The set is per platform. `SourceKind` is not: every variant exists on
//! every build so the page's types are checked whole. See the spec's
//! "SourceKind keeps every variant on both platforms".

#[cfg(unix)]
pub mod linux;
// Not gated: everything under it that calls the Windows API carries its own
// `#[cfg(windows)]`, so the pure halves stay testable on both platforms, the
// way `system::windows` does it. `linux` keeps its gate because its modules
// call unix-only APIs throughout.
pub mod windows;

use crate::Source;
use crate::appstream::Catalogue;
use crate::http::Client;
use crate::model::SystemInfo;
use std::sync::Arc;

pub fn all(
    system: &SystemInfo,
    client: Arc<Client>,
    catalogue: Arc<Catalogue>,
    preferences: &crate::Preferences,
) -> Vec<Box<dyn Source>> {
    #[cfg(unix)]
    {
        linux_all(system, client, catalogue, preferences)
    }
    #[cfg(windows)]
    {
        let _ = (system, client, catalogue, preferences);
        Vec::new()
    }
}

#[cfg(unix)]
fn linux_all(
    system: &SystemInfo,
    client: Arc<Client>,
    catalogue: Arc<Catalogue>,
    preferences: &crate::Preferences,
) -> Vec<Box<dyn Source>> {
    use linux::*;
    let mut aur = aur::Aur::new(system, client.clone(), catalogue.clone());
    // Automatic stays lazy: the helper is looked for on first use. A named
    // choice is applied now, falling back to what is installed when the
    // named one is not, so the setting never points at a missing program.
    if let Some(choice) = preferences.aur_helper.as_deref() {
        aur = aur.with_helper(aur::Helper::choose(choice));
    }
    let mut flatpak = flatpak::Flatpak::new(system, client.clone());
    if preferences.flatpak_user {
        flatpak.installation = flatpak::Installation::User;
    }
    vec![
        Box::new(pacman::Pacman::new(system, catalogue.clone())),
        Box::new(aur),
        Box::new(flatpak),
        Box::new(snap::Snap::new(system, client.clone())),
        Box::new(apt::Apt::new(system, catalogue.clone())),
        Box::new(dnf::Dnf::new(system, catalogue.clone())),
        Box::new(github::Github::new(
            system,
            client.clone(),
            catalogue.clone(),
        )),
        Box::new(fwupd::Fwupd::new(system)),
        Box::new(chwd::Chwd::new(system)),
    ]
}

#[cfg(test)]
mod tests {
    /// Until Task 8 there is no Windows source. The point of the test is
    /// that `all` answers rather than panicking or being absent, so the
    /// application and the text mode both run on Windows from here on.
    #[cfg(windows)]
    #[test]
    fn windows_has_no_sources_yet() {
        let system = crate::system::detect();
        let sources = super::all(
            &system,
            crate::http::Client::shared(),
            crate::appstream::Catalogue::load_system(&system),
            &crate::Preferences::default(),
        );
        assert!(sources.is_empty());
    }
}
