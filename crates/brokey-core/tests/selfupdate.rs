//! The self-updater, tested by what it says and by what it must never say.
//!
//! Three tables: every installation [`detect`] can return, from a probe of
//! fixed readings; every `(installation, architecture, asset list)`
//! combination through [`remedy`]; and the guard the style guide demands
//! (§18.3): the commands that used to be printed to machines with nowhere to
//! upgrade from, which must not appear in any sentence, and the rule that no
//! remedy names a file the release does not carry.

use brokey_core::model::SourceKind;
use brokey_core::selfupdate::{
    Arch, Asset, Format, Installation, Installer, Probe, Release, Remedy, Version, assemble,
    detect, plan, release, remedy,
};
use std::path::{Path, PathBuf};

const LATEST: &str = include_str!("fixtures/selfupdate/latest.json");
const LATEST_X86_ONLY: &str = include_str!("fixtures/selfupdate/latest-x86-only.json");
const NOT_FOUND: &str = include_str!("fixtures/selfupdate/not-found.json");

fn latest() -> Release {
    release::parse(LATEST)
        .expect("the fixture parses")
        .expect("the fixture is a release")
}

fn x86_only() -> Release {
    release::parse(LATEST_X86_ONLY)
        .expect("the fixture parses")
        .expect("the fixture is a release")
}

fn v(text: &str) -> Version {
    Version::parse(text).expect("a version")
}

fn no_env() -> Vec<(&'static str, &'static str)> {
    vec![("HOME", "/home/a")]
}

// --- detect -----------------------------------------------------------------

/// One imaginary machine: a name, the executable's path, the environment,
/// the paths that exist, and what [`detect`] must say.
type Case<'a> = (
    &'a str,
    &'a str,
    &'a [(&'a str, &'a str)],
    &'a [&'a str],
    Installation,
);

#[test]
fn every_installation_is_detected_from_its_own_readings() {
    let home = no_env();
    let appimage = [
        ("HOME", "/home/a"),
        ("APPIMAGE", "/home/a/Apps/Brokey-0.1.0-x86_64.AppImage"),
    ];
    let flatpak = [
        ("HOME", "/home/a"),
        ("FLATPAK_ID", "io.github.spillebulle.brokey"),
    ];
    let cases: Vec<Case<'_>> = vec![
        // Flatpak wins over everything that looks at paths: the host's dpkg
        // database can be visible through the runtime, and a Flatpak on
        // Ubuntu must not be told to run apt.
        (
            "flatpak by id",
            "/app/bin/brokey",
            &flatpak,
            &["/var/lib/dpkg/status"],
            Installation::Flatpak,
        ),
        (
            "flatpak by the runtime's file",
            "/app/bin/brokey",
            &home,
            &["/.flatpak-info", "/var/lib/pacman"],
            Installation::Flatpak,
        ),
        // An AppImage runs from a mount that vanishes; the image's own path
        // has to come from the variable.
        (
            "appimage",
            "/tmp/.mount_Brokey1a/usr/bin/brokey",
            &appimage,
            &["/home/a/Apps/Brokey-0.1.0-x86_64.AppImage"],
            Installation::AppImage {
                path: PathBuf::from("/home/a/Apps/Brokey-0.1.0-x86_64.AppImage"),
            },
        ),
        // The variable names a file that has gone. Writing a new one there
        // would leave a copy nobody asked for and the running one stale.
        (
            "appimage whose file has gone",
            "/tmp/.mount_Brokey1a/usr/bin/brokey",
            &appimage,
            &[],
            Installation::Unknown,
        ),
        (
            "aur package",
            "/usr/bin/brokey",
            &home,
            &["/var/lib/pacman", "/var/lib/pacman/local/brokey-*"],
            Installation::Pacman {
                package: "brokey".into(),
            },
        ),
        // Both wildcards match a bin install, so the bin one has to win.
        (
            "bin package",
            "/usr/bin/brokey",
            &home,
            &[
                "/var/lib/pacman",
                "/var/lib/pacman/local/brokey-*",
                "/var/lib/pacman/local/brokey-bin-*",
            ],
            Installation::Pacman {
                package: "brokey-bin".into(),
            },
        ),
        (
            "arch with no record of us",
            "/usr/bin/brokey",
            &home,
            &["/var/lib/pacman"],
            Installation::Unknown,
        ),
        (
            "debian without the archive",
            "/usr/bin/brokey",
            &home,
            &["/var/lib/dpkg/status"],
            Installation::Dpkg { archive: false },
        ),
        (
            "debian with the archive",
            "/usr/bin/brokey",
            &home,
            &[
                "/var/lib/dpkg/status",
                "/etc/apt/sources.list.d/spillebulle.sources",
            ],
            Installation::Dpkg { archive: true },
        ),
        // One manager's archive is not the other's: a stray .repo on Debian
        // has not pointed apt anywhere.
        (
            "debian with the wrong archive file",
            "/usr/bin/brokey",
            &home,
            &["/var/lib/dpkg/status", "/etc/yum.repos.d/spillebulle.repo"],
            Installation::Dpkg { archive: false },
        ),
        (
            "fedora without the archive",
            "/usr/bin/brokey",
            &home,
            &["/var/lib/rpm"],
            Installation::Rpm { archive: false },
        ),
        (
            "fedora with the newer database location and the archive",
            "/usr/bin/brokey",
            &home,
            &["/usr/lib/sysimage/rpm", "/etc/yum.repos.d/spillebulle.repo"],
            Installation::Rpm { archive: true },
        ),
        (
            "portable under home",
            "/home/a/brokey-0.1.0/brokey",
            &home,
            &["/var/lib/pacman", "/var/lib/pacman/local/brokey-*"],
            Installation::Portable,
        ),
        (
            "portable under opt",
            "/opt/brokey/brokey",
            &home,
            &["/var/lib/dpkg/status"],
            Installation::Portable,
        ),
        (
            "portable under tmp",
            "/tmp/brokey",
            &home,
            &[],
            Installation::Portable,
        ),
        // /usr/local exists precisely so locally installed software has
        // somewhere no package manager touches.
        (
            "portable under usr local",
            "/usr/local/bin/brokey",
            &home,
            &["/var/lib/rpm"],
            Installation::Portable,
        ),
        (
            "system path with no manager",
            "/usr/bin/brokey",
            &home,
            &[],
            Installation::Unknown,
        ),
        (
            "somewhere else entirely",
            "/srv/tools/brokey",
            &home,
            &["/var/lib/pacman"],
            Installation::Unknown,
        ),
        (
            "no path at all",
            "",
            &home,
            &["/var/lib/pacman"],
            Installation::Unknown,
        ),
    ];
    for (name, exe, env, present, expected) in cases {
        let probe = Probe::fixed(exe, env, present);
        assert_eq!(detect(&probe), expected, "{name}");
    }
}

#[test]
fn a_home_directory_is_read_from_the_environment_not_assumed() {
    // Without HOME nothing under /home is special.
    let probe = Probe::fixed("/home/a/brokey", &[], &[]);
    assert_eq!(detect(&probe), Installation::Unknown);
    // And an unusual home is honoured.
    let probe = Probe::fixed(
        "/data/people/a/bin/brokey",
        &[("HOME", "/data/people/a")],
        &[],
    );
    assert_eq!(detect(&probe), Installation::Portable);
}

// --- release ----------------------------------------------------------------

#[test]
fn the_fixture_reads_as_the_release_it_is() {
    let r = latest();
    assert_eq!(r.version, v("0.2.0"));
    assert_eq!(r.tag, "v0.2.0");
    assert_eq!(r.name, "Brokey 0.2.0");
    assert_eq!(
        r.url,
        "https://github.com/Spillebulle/Brokey/releases/tag/v0.2.0"
    );
    assert!(r.notes.starts_with("## 0.2.0"));
    // 2026-10-01T10:15:30Z
    assert_eq!(r.published, Some(1_790_849_730));
    assert_eq!(r.assets.len(), 9);
    assert!(r.assets.iter().all(Asset::is_fetchable));
}

#[test]
fn the_404_body_is_not_a_release_and_not_a_guess() {
    // The body GitHub sends with a 404. `latest()` turns the status into
    // `Ok(None)` before this is ever parsed; if it were parsed it must be
    // an error, never "no update".
    assert!(release::parse(NOT_FOUND).is_err());
}

#[test]
fn every_documented_asset_name_is_in_the_fixture_and_is_found() {
    // The names in `selfupdate/mod.rs`'s table, spelt by `Format::name`,
    // against the fixture that stands in for a real release. If the
    // workflow changes a name, this is where it shows.
    let r = latest();
    let version = &r.version;
    let expected = [
        (Format::Deb, Arch::X86_64, "brokey_0.2.0_amd64.deb"),
        (Format::Deb, Arch::Aarch64, "brokey_0.2.0_arm64.deb"),
        (Format::Rpm, Arch::X86_64, "brokey-0.2.0-1.x86_64.rpm"),
        (Format::Rpm, Arch::Aarch64, "brokey-0.2.0-1.aarch64.rpm"),
        (
            Format::PacmanPackage,
            Arch::X86_64,
            "brokey-bin-0.2.0-1-x86_64.pkg.tar.zst",
        ),
        (
            Format::AppImage,
            Arch::X86_64,
            "Brokey-0.2.0-x86_64.AppImage",
        ),
        (
            Format::AppImage,
            Arch::Aarch64,
            "Brokey-0.2.0-aarch64.AppImage",
        ),
        (Format::Flatpak, Arch::X86_64, "brokey-0.2.0-x86_64.flatpak"),
    ];
    for (format, arch, name) in expected {
        assert_eq!(format.name(version, arch), name);
        let found = brokey_core::selfupdate::remedy::find(&r.assets, format, Some(arch))
            .unwrap_or_else(|| panic!("{format:?} {arch:?} not found in the fixture"));
        assert_eq!(found.name, name);
    }
    // The ones the table says are not built.
    assert_eq!(
        brokey_core::selfupdate::remedy::find(
            &r.assets,
            Format::PacmanPackage,
            Some(Arch::Aarch64)
        ),
        None
    );
    assert_eq!(
        brokey_core::selfupdate::remedy::find(&r.assets, Format::Flatpak, Some(Arch::Aarch64)),
        None
    );
    assert_eq!(
        brokey_core::selfupdate::remedy::find(&r.assets, Format::Deb, None),
        None
    );
}

// --- remedy -----------------------------------------------------------------

fn every_installation() -> Vec<Installation> {
    vec![
        Installation::Flatpak,
        Installation::AppImage {
            path: PathBuf::from("/home/a/Apps/Brokey-0.1.0-x86_64.AppImage"),
        },
        Installation::Pacman {
            package: "brokey".into(),
        },
        Installation::Pacman {
            package: "brokey-bin".into(),
        },
        Installation::Dpkg { archive: true },
        Installation::Dpkg { archive: false },
        Installation::Rpm { archive: true },
        Installation::Rpm { archive: false },
        Installation::Portable,
        Installation::Unknown,
    ]
}

/// The architectures a remedy can be asked about: the two that are built,
/// one that is not, and nonsense.
const ARCHES: [&str; 4] = ["x86_64", "aarch64", "riscv64", ""];

/// Every asset list a release might carry: everything, x86-64 only with a
/// Flatpak on plain http, and nothing at all.
fn every_asset_list() -> Vec<(&'static str, Vec<Asset>)> {
    vec![
        ("full", latest().assets),
        ("x86 only", x86_only().assets),
        ("empty", Vec::new()),
    ]
}

fn has_archive(installation: &Installation) -> bool {
    matches!(
        installation,
        Installation::Dpkg { archive: true } | Installation::Rpm { archive: true }
    )
}

/// The file names a sentence could carry, by extension. Any word ending in
/// one of these is a claim that the file exists.
fn named_files(sentence: &str) -> Vec<String> {
    sentence
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
        .map(|w| w.trim_end_matches('.'))
        // A path on this machine (the AppImage being replaced) is a fact
        // about the machine, not a claim about the release.
        .filter(|w| !w.contains('/'))
        .filter(|w| {
            [".deb", ".rpm", ".pkg.tar.zst", ".AppImage", ".flatpak"]
                .iter()
                .any(|ext| w.ends_with(ext))
        })
        .map(str::to_string)
        .collect()
}

/// **The regression this whole arrangement exists for.** Muster 0.0.8 told
/// every Debian user to run `apt install --only-upgrade`, which cannot ever
/// have worked, and answered "already the newest version" in the voice of
/// something that had checked. Nothing here may print an upgrade command to
/// a machine that has nowhere to upgrade from.
#[test]
fn no_upgrade_command_reaches_a_machine_without_an_archive() {
    const FORBIDDEN: [&str; 5] = [
        "apt install --only-upgrade",
        "pacman -Syu brokey",
        "flatpak update",
        "dnf upgrade brokey",
        "already the newest",
    ];
    for installation in every_installation() {
        if has_archive(&installation) {
            continue;
        }
        for arch in ARCHES {
            for (list, assets) in every_asset_list() {
                let r = remedy(&installation, &v("0.2.0"), arch, &assets);
                for command in FORBIDDEN {
                    assert!(
                        !r.sentence().contains(command),
                        "{installation:?} on {arch:?} with {list} assets was told `{command}`:\n{}",
                        r.sentence()
                    );
                }
            }
        }
    }
}

#[test]
fn no_remedy_names_a_file_the_release_does_not_carry() {
    for installation in every_installation() {
        for arch in ARCHES {
            for (list, assets) in every_asset_list() {
                let r = remedy(&installation, &v("0.2.0"), arch, &assets);
                let carried: Vec<&str> = assets
                    .iter()
                    .filter(|a| a.is_fetchable())
                    .map(|a| a.name.as_str())
                    .collect();
                for file in named_files(r.sentence()) {
                    assert!(
                        carried.contains(&file.as_str()),
                        "{installation:?} on {arch:?} with {list} assets named {file}, which is not in the release:\n{}",
                        r.sentence()
                    );
                }
                if let Some(asset) = r.asset() {
                    assert!(
                        carried.contains(&asset),
                        "{installation:?} on {arch:?} with {list}: asset {asset}"
                    );
                    assert!(
                        r.sentence().contains(asset),
                        "{installation:?}: the sentence should name the file it fetches:\n{}",
                        r.sentence()
                    );
                }
                // An architecture with nothing built gets a sentence and no
                // name, ever.
                if Arch::parse(arch).is_none() || list == "empty" {
                    assert!(
                        named_files(r.sentence()).is_empty(),
                        "{installation:?} on {arch:?} with {list} assets named a file:\n{}",
                        r.sentence()
                    );
                    assert_eq!(r.asset(), None);
                }
            }
        }
    }
}

#[test]
fn every_sentence_is_a_sentence_in_the_house_voice() {
    for installation in every_installation() {
        for arch in ARCHES {
            for (_, assets) in every_asset_list() {
                let r = remedy(&installation, &v("0.2.0"), arch, &assets);
                let s = r.sentence();
                assert!(
                    s.ends_with('.'),
                    "{installation:?} on {arch:?}: no full stop:\n{s}"
                );
                assert!(
                    !s.contains('\u{2014}'),
                    "{installation:?}: an em dash:\n{s}"
                );
                assert!(!s.contains("  "), "{installation:?}: a double space:\n{s}");
                assert!(
                    s.starts_with(|c: char| c.is_ascii_uppercase()),
                    "{installation:?}:\n{s}"
                );
            }
        }
    }
}

#[test]
fn each_installation_gets_the_rule_the_style_guide_settles() {
    let assets = latest().assets;
    let x86 = "x86_64";
    let latest = v("0.2.0");

    // The AUR package is an ordinary row on the Updates page.
    let r = remedy(
        &Installation::Pacman {
            package: "brokey".into(),
        },
        &latest,
        x86,
        &assets,
    );
    assert_eq!(
        r,
        Remedy::UpdatesPage {
            source: SourceKind::Aur,
            package: "brokey".into(),
            sentence: "Brokey 0.2.0 is published. This copy updates through the AUR package \
                       brokey, which appears in Updates once the AUR has it."
                .into(),
        }
    );
    // On any architecture, and with no assets: the AUR builds from source.
    for arch in ARCHES {
        let r = remedy(
            &Installation::Pacman {
                package: "brokey".into(),
            },
            &latest,
            arch,
            &[],
        );
        assert!(
            matches!(
                r,
                Remedy::UpdatesPage {
                    source: SourceKind::Aur,
                    ..
                }
            ),
            "{arch}: {r:?}"
        );
    }

    // The bin package is the release asset through pacman -U.
    let r = remedy(
        &Installation::Pacman {
            package: "brokey-bin".into(),
        },
        &latest,
        x86,
        &assets,
    );
    match &r {
        Remedy::InstallAsset {
            asset,
            url,
            installer,
            ..
        } => {
            assert_eq!(asset, "brokey-bin-0.2.0-1-x86_64.pkg.tar.zst");
            assert!(url.ends_with("/brokey-bin-0.2.0-1-x86_64.pkg.tar.zst"));
            assert_eq!(*installer, Installer::PacmanU);
        }
        other => panic!("{other:?}"),
    }
    // No ARM build of it: a sentence, no name.
    let r = remedy(
        &Installation::Pacman {
            package: "brokey-bin".into(),
        },
        &latest,
        "aarch64",
        &assets,
    );
    assert!(matches!(r, Remedy::Sentence { .. }), "{r:?}");

    // With the archive, apt and dnf have it.
    let r = remedy(&Installation::Dpkg { archive: true }, &latest, x86, &assets);
    assert!(
        matches!(
            r,
            Remedy::UpdatesPage {
                source: SourceKind::Apt,
                ..
            }
        ),
        "{r:?}"
    );
    let r = remedy(&Installation::Rpm { archive: true }, &latest, x86, &assets);
    assert!(
        matches!(
            r,
            Remedy::UpdatesPage {
                source: SourceKind::Dnf,
                ..
            }
        ),
        "{r:?}"
    );

    // Without it, the exact package.
    let r = remedy(
        &Installation::Dpkg { archive: false },
        &latest,
        x86,
        &assets,
    );
    assert!(
        matches!(&r, Remedy::InstallAsset { asset, installer: Installer::DpkgI, .. } if asset == "brokey_0.2.0_amd64.deb"),
        "{r:?}"
    );
    let r = remedy(
        &Installation::Dpkg { archive: false },
        &latest,
        "aarch64",
        &assets,
    );
    assert!(
        matches!(&r, Remedy::InstallAsset { asset, installer: Installer::DpkgI, .. } if asset == "brokey_0.2.0_arm64.deb"),
        "{r:?}"
    );
    let r = remedy(&Installation::Rpm { archive: false }, &latest, x86, &assets);
    assert!(
        matches!(&r, Remedy::InstallAsset { asset, installer: Installer::RpmU, .. } if asset == "brokey-0.2.0-1.x86_64.rpm"),
        "{r:?}"
    );
    let r = remedy(
        &Installation::Rpm { archive: false },
        &latest,
        "aarch64",
        &assets,
    );
    assert!(
        matches!(&r, Remedy::InstallAsset { asset, installer: Installer::RpmU, .. } if asset == "brokey-0.2.0-1.aarch64.rpm"),
        "{r:?}"
    );

    // The Flatpak is a sentence naming the bundle, and no command.
    let r = remedy(&Installation::Flatpak, &latest, x86, &assets);
    assert_eq!(
        r,
        Remedy::Sentence {
            sentence: "This copy runs as a Flatpak bundle with no remote behind it. \
                       Take brokey-0.2.0-x86_64.flatpak from the releases page and install it over this one."
                .into()
        }
    );
    let r = remedy(&Installation::Flatpak, &latest, "aarch64", &assets);
    assert!(
        matches!(r, Remedy::Sentence { .. }) && !r.sentence().contains(".flatpak"),
        "{r:?}"
    );

    // The AppImage is swapped in place.
    let path = PathBuf::from("/home/a/Apps/Brokey-0.1.0-x86_64.AppImage");
    let r = remedy(
        &Installation::AppImage { path: path.clone() },
        &latest,
        x86,
        &assets,
    );
    match &r {
        Remedy::ReplaceFile {
            path: p,
            asset,
            url,
            ..
        } => {
            assert_eq!(p, &path);
            assert_eq!(asset, "Brokey-0.2.0-x86_64.AppImage");
            assert!(url.starts_with("https://"));
        }
        other => panic!("{other:?}"),
    }
    let r = remedy(
        &Installation::AppImage { path: path.clone() },
        &latest,
        "aarch64",
        &assets,
    );
    assert!(
        matches!(&r, Remedy::ReplaceFile { asset, .. } if asset == "Brokey-0.2.0-aarch64.AppImage"),
        "{r:?}"
    );

    // A portable copy is told about the AppImage: there is no tarball.
    let r = remedy(&Installation::Portable, &latest, x86, &assets);
    assert!(matches!(r, Remedy::Sentence { .. }), "{r:?}");
    assert!(
        r.sentence().contains("Brokey-0.2.0-x86_64.AppImage"),
        "{}",
        r.sentence()
    );
    assert!(!r.sentence().contains(".tar.gz"));

    // Unknown says so and names nothing.
    let r = remedy(&Installation::Unknown, &latest, x86, &assets);
    assert_eq!(
        r,
        Remedy::Sentence {
            sentence: "This copy was installed in a way Brokey cannot update by itself. \
                       Update it the way it was installed."
                .into()
        }
    );
}

#[test]
fn a_release_that_built_x86_only_leaves_arm_with_a_sentence() {
    let assets = x86_only().assets;
    for installation in every_installation() {
        let r = remedy(&installation, &v("0.3.0"), "aarch64", &assets);
        if has_archive(&installation)
            || matches!(installation, Installation::Pacman { ref package } if package == "brokey")
        {
            assert!(
                matches!(r, Remedy::UpdatesPage { .. }),
                "{installation:?}: {r:?}"
            );
        } else {
            assert!(
                matches!(r, Remedy::Sentence { .. }),
                "{installation:?}: {r:?}"
            );
            assert!(
                named_files(r.sentence()).is_empty(),
                "{installation:?}: {}",
                r.sentence()
            );
        }
    }
    // And the Flatpak on plain http is treated as absent even on x86-64.
    let r = remedy(&Installation::Flatpak, &v("0.3.0"), "x86_64", &assets);
    assert!(!r.sentence().contains(".flatpak"), "{}", r.sentence());
}

// --- assemble and plan ------------------------------------------------------

#[test]
fn a_newer_release_carries_a_remedy_and_an_older_one_does_not() {
    let r = latest();
    let installation = Installation::Pacman {
        package: "brokey-bin".into(),
    };

    let s = assemble(&v("0.1.0"), Some(&r), installation.clone(), "x86_64", None);
    assert_eq!(s.current, "0.1.0");
    let l = s.latest.as_ref().expect("a latest");
    assert_eq!(l.version, "0.2.0");
    assert!(l.newer);
    assert_eq!(l.url, r.url);
    assert_eq!(l.published, r.published);
    assert!(s.remedy.as_ref().is_some_and(Remedy::is_actionable));
    assert_eq!(s.installation, installation);
    assert_eq!(s.installation_label, "the brokey-bin package");
    assert_eq!(s.error, None);

    // The same version: nothing to do, and the release is still shown.
    let s = assemble(&v("0.2.0"), Some(&r), installation.clone(), "x86_64", None);
    assert!(s.latest.as_ref().is_some_and(|l| !l.newer));
    assert_eq!(s.remedy, None);

    // A development build ahead of the release: also nothing.
    let s = assemble(&v("0.3.0"), Some(&r), installation.clone(), "x86_64", None);
    assert_eq!(s.remedy, None);

    // A candidate of the released version is older than it.
    let s = assemble(
        &v("0.2.0-rc.1"),
        Some(&r),
        installation.clone(),
        "x86_64",
        None,
    );
    assert!(s.remedy.is_some());

    // No release yet, and GitHub unreachable: both say so without a remedy.
    let s = assemble(&v("0.1.0"), None, installation.clone(), "x86_64", None);
    assert_eq!(s.latest, None);
    assert_eq!(s.remedy, None);
    let s = assemble(
        &v("0.1.0"),
        None,
        installation,
        "x86_64",
        Some("Could not reach api.github.com.".into()),
    );
    assert_eq!(s.error.as_deref(), Some("Could not reach api.github.com."));
}

#[test]
fn the_check_serialises_with_lower_case_tags_for_the_page() {
    let s = assemble(
        &v("0.1.0"),
        Some(&latest()),
        Installation::Dpkg { archive: false },
        "x86_64",
        None,
    );
    let json = serde_json::to_value(&s).expect("serialises");
    assert_eq!(json["installation"]["kind"], "dpkg");
    assert_eq!(json["installation"]["archive"], false);
    assert_eq!(json["remedy"]["kind"], "install_asset");
    assert_eq!(json["remedy"]["installer"], "dpkgi");
    assert_eq!(json["latest"]["newer"], true);
    assert!(
        json["remedy"]["sentence"]
            .as_str()
            .is_some_and(|s| s.ends_with('.'))
    );
    let back: brokey_core::selfupdate::SelfUpdate =
        serde_json::from_value(json).expect("round trips");
    assert_eq!(back, s);
}

#[test]
fn an_install_asset_plans_a_download_and_a_root_install() {
    let dir = Path::new("/home/a/.cache/brokey/http/downloads");
    let cases = [
        (
            Installation::Pacman {
                package: "brokey-bin".into(),
            },
            "pacman",
            vec!["-U", "--noconfirm"],
            SourceKind::Pacman,
            true,
        ),
        (
            Installation::Dpkg { archive: false },
            "apt-get",
            vec!["install", "-y"],
            SourceKind::Apt,
            true,
        ),
        (
            Installation::Rpm { archive: false },
            "dnf",
            vec!["install", "-y"],
            SourceKind::Dnf,
            true,
        ),
    ];
    for (installation, program, leading, source, root) in cases {
        let r = remedy(&installation, &v("0.2.0"), "x86_64", &latest().assets);
        let asset = r.asset().expect("an asset").to_string();
        let url = match &r {
            Remedy::InstallAsset { url, .. } => url.clone(),
            other => panic!("{other:?}"),
        };
        let steps = plan(&r, dir).expect("a plan");
        assert_eq!(steps.len(), 2, "{installation:?}");
        let file = dir.join(&asset).display().to_string();

        let download = &steps[0];
        assert_eq!(download.source, SourceKind::Github);
        assert_eq!(download.command.program, "curl");
        assert_eq!(
            download.command.args,
            ["-L", "--fail", "--create-dirs", "-o", &file, &url]
        );
        assert!(!download.needs_root);
        assert_eq!(download.title, format!("Downloading {asset}"));

        let install = &steps[1];
        assert_eq!(install.source, source);
        assert_eq!(install.command.program, program);
        let mut expected: Vec<String> = leading.iter().map(|s| s.to_string()).collect();
        expected.push(file.clone());
        assert_eq!(install.command.args, expected, "{installation:?}");
        assert_eq!(install.needs_root, root);
        assert!(
            install.title.starts_with("Installing "),
            "{}",
            install.title
        );
        if program == "apt-get" {
            assert_eq!(
                install.command.env,
                vec![("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]
            );
        }
    }
}

#[test]
fn a_replace_file_plans_a_download_and_an_install_of_the_file_as_the_user() {
    let dir = Path::new("/home/a/.cache/brokey/http/downloads");
    let path = PathBuf::from("/home/a/Apps/Brokey-0.1.0-x86_64.AppImage");
    let r = remedy(
        &Installation::AppImage { path: path.clone() },
        &v("0.2.0"),
        "x86_64",
        &latest().assets,
    );
    let steps = plan(&r, dir).expect("a plan");
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].command.program, "curl");
    let downloaded = dir
        .join("Brokey-0.2.0-x86_64.AppImage")
        .display()
        .to_string();
    assert_eq!(steps[1].command.program, "install");
    assert_eq!(
        steps[1].command.args,
        ["-m755", &downloaded, &path.display().to_string()]
    );
    assert!(steps.iter().all(|s| !s.needs_root));
}

#[test]
fn a_remedy_that_is_not_actionable_has_no_plan_and_says_why() {
    let dir = Path::new("/x");
    for installation in [
        Installation::Pacman {
            package: "brokey".into(),
        },
        Installation::Dpkg { archive: true },
        Installation::Flatpak,
        Installation::Portable,
        Installation::Unknown,
    ] {
        let r = remedy(&installation, &v("0.2.0"), "x86_64", &latest().assets);
        assert!(!r.is_actionable(), "{installation:?}");
        let err = plan(&r, dir).expect_err("no plan");
        assert_eq!(err.message, r.sentence(), "{installation:?}");
    }
}

#[test]
fn the_flatpak_bundle_installer_plans_as_the_user_if_ever_used() {
    // No remedy produces it today (a sandboxed store cannot run flatpak),
    // but the plan has an answer so a future unsandboxed bundle is not a
    // panic.
    let r = Remedy::InstallAsset {
        asset: "brokey-0.2.0-x86_64.flatpak".into(),
        url: "https://github.com/x/brokey-0.2.0-x86_64.flatpak".into(),
        installer: Installer::FlatpakBundle,
        sentence: "S.".into(),
    };
    let dir = Path::new("/d");
    let steps = plan(&r, dir).expect("a plan");
    assert_eq!(steps[1].command.program, "flatpak");
    // Joined rather than a literal: `display` renders the platform's own
    // separator, which the plan's own file path also went through.
    let file = dir
        .join("brokey-0.2.0-x86_64.flatpak")
        .display()
        .to_string();
    assert_eq!(steps[1].command.args, ["install", "--user", "-y", &file]);
    assert!(!steps[1].needs_root);
    assert_eq!(steps[1].source, SourceKind::Flatpak);
}

// --- live -------------------------------------------------------------------

/// Asks GitHub for the newest release of this repository. Either answer is
/// right: `None` until the first release is cut, `Some` after. What must
/// not happen is an error on a repository that simply has no release yet.
#[test]
#[ignore]
fn live_latest_release_is_none_or_a_version() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let client = brokey_core::http::Client::new(dir.path().to_path_buf());
    match release::latest(&client, false) {
        Ok(None) => eprintln!("no release yet: Ok(None)"),
        Ok(Some(r)) => {
            eprintln!("newest release: {} ({} assets)", r.version, r.assets.len());
            assert!(r.url.starts_with("https://github.com/Spillebulle/Brokey/"));
        }
        Err(e) => panic!("{e}"),
    }
}
