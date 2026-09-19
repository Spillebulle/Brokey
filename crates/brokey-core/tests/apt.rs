//! The apt source against hand-written Packages lists and a dpkg status
//! file, and dpkg's version ordering against a table written from the
//! Debian policy manual (§5.6.12) and `dpkg --compare-versions`. Nothing
//! here reads the machine's own `/var/lib`; on a machine that is not
//! Debian-based the source must say so, and that is tested too.

#![cfg(unix)]

use brokey_core::appstream::Catalogue;
use brokey_core::sources::linux::apt::{
    Apt, ListName, dpkg_compare, dpkg_is_newer, parse_control, parse_progress, parse_simulation,
};
use brokey_core::{Op, PackageKind, PackageRef, Query, Source, SourceKind, SystemInfo};
use std::cmp::Ordering;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/apt")
}

fn debian() -> SystemInfo {
    SystemInfo {
        distro_id: "debian".to_string(),
        distro_like: Vec::new(),
        pretty_name: "Debian GNU/Linux 12 (bookworm)".to_string(),
        arch: "x86_64".to_string(),
        desktop: None,
        session: None,
    }
}

fn cachyos() -> SystemInfo {
    SystemInfo {
        distro_id: "cachyos".to_string(),
        distro_like: vec!["arch".to_string()],
        pretty_name: "CachyOS".to_string(),
        arch: "x86_64".to_string(),
        desktop: Some("COSMIC".to_string()),
        session: Some("wayland".to_string()),
    }
}

fn catalogue() -> Arc<Catalogue> {
    Arc::new(Catalogue::default())
}

fn apt_get() -> Option<PathBuf> {
    Some(PathBuf::from("/usr/bin/apt-get"))
}

/// The source as a Debian machine with the fixture lists would build it,
/// with apt's upgrade simulation answered from what apt-get printed for them.
fn source() -> Apt {
    let recorded = std::fs::read_to_string(fixtures().join("simulate-upgrade")).unwrap();
    source_simulating(recorded)
}

fn source_simulating(simulation: impl Into<String>) -> Apt {
    let simulation = simulation.into();
    Apt::at(
        &debian(),
        catalogue(),
        fixtures().join("lists"),
        fixtures().join("status"),
        apt_get(),
    )
    .with_simulation(Box::new(move || Ok(simulation.clone())))
}

fn apt_ref(name: &str) -> PackageRef {
    PackageRef {
        source: SourceKind::Apt,
        id: name.to_string(),
    }
}

fn sign(o: Ordering) -> i8 {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

#[test]
fn dpkg_compare_agrees_with_dpkg() {
    // Each row is what `dpkg --compare-versions a gt|eq|lt b` answers. The
    // rules come from policy §5.6.12: epoch first; then upstream, then
    // revision, each as alternating non-digit and digit runs; `~` sorts
    // before everything including the end of the part; letters sort
    // before non-letters; an absent revision or epoch is 0; the last
    // hyphen splits the revision off.
    const TABLE: &[(&str, &str, i8)] = &[
        ("1.0", "1.0", 0),
        ("1.0-1", "1.0-2", -1),
        ("1.0-1", "1.0", 1),
        ("1.0", "1.0-0", 0),
        ("1:1.0", "2.0", 1),
        ("1:1.0", "1:1.1", -1),
        ("0:1.0", "1.0", 0),
        ("2:1.0", "1:9.9", 1),
        ("1:1.0-1", "1.0-2", 1),
        // The manual's own ladder: ~~ then ~~a then ~ then nothing then a.
        ("1.0~~", "1.0~~a", -1),
        ("1.0~~a", "1.0~", -1),
        ("1.0~", "1.0", -1),
        ("1.0", "1.0a", -1),
        ("1.0~rc1", "1.0", -1),
        ("1.0~rc1", "1.0~rc2", -1),
        ("1.0rc1", "1.0", 1),
        ("1.2.3", "1.2.3~beta1", 1),
        ("1.0a", "1.0b", -1),
        ("1.0a", "1.0+", -1),
        ("1.0+dfsg", "1.0.1", -1),
        ("1.0+dfsg-1", "1.0-1", 1),
        ("1.0+really0.9", "1.0", 1),
        ("1.0+really0.9", "1.1", -1),
        ("1.2.3+git20240101", "1.2.3", 1),
        ("1.10", "1.9", 1),
        ("1.01", "1.1", 0),
        ("12.04", "12.4", 0),
        ("1.0", "1.00", 0),
        ("1.0.0", "1.0", 1),
        ("1.0", "1.0.a", -1),
        ("1.0.a", "1.0.b", -1),
        ("2.6.32", "2.6.9", 1),
        ("0.9", "1.0", -1),
        ("1.0.1", "1.0.1-1", -1),
        ("2.10.34-1+deb12u2", "2.10.34-1+deb12u1", 1),
        ("115.8.0esr-1~deb12u1", "115.6.0esr-1~deb12u1", 1),
        ("1.0-1ubuntu1", "1.0-1", 1),
        ("1.0-1ubuntu1", "1.0-2", -1),
        ("1.0-1~bpo12+1", "1.0-1", -1),
        ("1.0-1+b1", "1.0-1", 1),
        ("1.0-1+b1", "1.0-1.1", -1),
        ("2.0-0ubuntu1", "2.0", 1),
        ("1.0-1", "1.0-1~", 1),
        ("1.0-1", "1.0-1a", -1),
        ("1.0-1-1", "1.0-2", 1),
    ];
    assert!(TABLE.len() >= 30);
    for (a, b, want) in TABLE {
        assert_eq!(sign(dpkg_compare(a, b)), *want, "dpkg_compare({a}, {b})");
        assert_eq!(sign(dpkg_compare(b, a)), -*want, "dpkg_compare({b}, {a})");
    }
    assert!(dpkg_is_newer("1.0", "1.0~rc1"));
    assert!(!dpkg_is_newer("1.0", "1.0"));
}

#[test]
fn on_this_machine_apt_says_why_it_is_unavailable() {
    let system = brokey_core::system::detect();
    let apt = Apt::new(&system, catalogue());
    let status = apt.status();
    assert_eq!(status.kind, SourceKind::Apt);
    if system.is_debian_like() {
        // CI's Ubuntu runner: the real apt-get, run without root, answers
        // the upgrade simulation, and every upgrade read from it is newer
        // than what is installed. The fixtures carry the rest.
        if status.available {
            for update in apt.updates().expect("apt answers the upgrade simulation") {
                let from = update.from.as_deref().unwrap_or("");
                assert!(
                    dpkg_is_newer(&update.to, from),
                    "{} {from} -> {}",
                    update.name,
                    update.to
                );
            }
        }
        return;
    }
    assert!(!status.available);
    assert_eq!(
        status.reason,
        Some(format!(
            "apt is for Debian-based systems; this is {}.",
            system.pretty_name
        ))
    );
    assert_eq!(status.detail, None);
    let err = apt.search(&Query::new("gimp")).unwrap_err();
    assert_eq!(err.source_kind, Some(SourceKind::Apt));
    assert_eq!(Some(err.message), status.reason);
}

#[test]
fn a_cachyos_machine_is_told_apt_is_for_debian() {
    let apt = Apt::at(
        &cachyos(),
        catalogue(),
        fixtures().join("lists"),
        fixtures().join("status"),
        apt_get(),
    );
    let status = apt.status();
    assert!(!status.available);
    assert_eq!(
        status.reason.as_deref(),
        Some("apt is for Debian-based systems; this is CachyOS.")
    );
    for result in [apt.installed(), apt.search(&Query::new("gimp"))] {
        assert_eq!(
            result.unwrap_err().message,
            "apt is for Debian-based systems; this is CachyOS."
        );
    }
    assert_eq!(
        apt.updates().unwrap_err().message,
        "apt is for Debian-based systems; this is CachyOS."
    );
    assert_eq!(
        apt.details("gimp").unwrap_err().message,
        "apt is for Debian-based systems; this is CachyOS."
    );
    assert_eq!(
        apt.plan(&Op::Refresh {
            source: SourceKind::Apt
        })
        .unwrap_err()
        .message,
        "apt is for Debian-based systems; this is CachyOS."
    );
}

#[test]
fn a_debian_machine_without_apt_get_or_a_status_file_says_which() {
    let no_apt_get = Apt::at(
        &debian(),
        catalogue(),
        fixtures().join("lists"),
        fixtures().join("status"),
        None,
    );
    assert_eq!(
        no_apt_get.status().reason.as_deref(),
        Some("apt-get is not installed, so packages cannot be changed.")
    );
    let no_status = Apt::at(
        &debian(),
        catalogue(),
        fixtures().join("lists"),
        fixtures().join("no-such-status"),
        apt_get(),
    );
    let reason = no_status.status().reason.unwrap();
    assert!(
        reason.ends_with("no-such-status does not exist, so installed packages cannot be read."),
        "{reason}"
    );
}

#[test]
fn a_debian_machine_lists_its_suites_in_the_status_detail() {
    let status = source().status();
    assert!(status.available);
    assert_eq!(status.reason, None);
    assert_eq!(
        status.detail.as_deref(),
        Some("bookworm, bookworm-security")
    );
}

#[test]
fn search_ranks_the_name_first_and_reads_only_this_architecture() {
    let apt = source();
    let found = apt.search(&Query::new("gimp")).unwrap();
    let names: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(names, ["gimp", "gimp-data", "libgimp2.0", "krita"]);

    let gimp = &found[0];
    assert_eq!(gimp.source, SourceKind::Apt);
    assert_eq!(
        gimp.name, "gimp",
        "without a catalogue component the name is the package name"
    );
    assert_eq!(gimp.kind, PackageKind::Package);
    assert_eq!(gimp.version.as_deref(), Some("2.10.34-1+deb12u2"));
    assert!(gimp.installed);
    assert_eq!(gimp.installed_version.as_deref(), Some("2.10.34-1+deb12u1"));
    assert_eq!(gimp.repo.as_deref(), Some("bookworm/main"));
    assert_eq!(
        gimp.summary.as_deref(),
        Some("GNU Image Manipulation Program")
    );
    assert_eq!(gimp.homepage.as_deref(), Some("https://www.gimp.org/"));
    assert_eq!(gimp.download_size, Some(5_204_492));
    assert_eq!(gimp.installed_size, Some(20_260 * 1024));
    assert_eq!(
        gimp.description, None,
        "a search leaves the description to details"
    );
    assert!(gimp.facts.is_empty());
    assert_eq!(gimp.appstream_id, None);

    // krita is a hit through its description alone, so it comes last.
    let krita = &found[3];
    assert!(!krita.installed, "config-files is not installed");
    assert_eq!(krita.version.as_deref(), Some("1:5.1.5+dfsg-1"));

    // The i386 list is not for this machine.
    assert!(apt.search(&Query::new("wine")).unwrap().is_empty());
    // A blank query is nothing, not everything.
    assert!(apt.search(&Query::new("   ")).unwrap().is_empty());
}

#[test]
fn a_fonts_section_package_is_a_font() {
    let found = source().search(&Query::new("dejavu")).unwrap();
    assert_eq!(found[0].id, "fonts-dejavu");
    assert_eq!(found[0].kind, PackageKind::Font);
}

#[test]
fn search_finds_a_package_that_is_installed_but_gone_from_the_lists() {
    let found = source().search(&Query::new("old-tool")).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].installed);
    assert_eq!(found[0].installed_version.as_deref(), Some("1.0-1"));
    assert_eq!(found[0].version, None);
    assert_eq!(found[0].repo, None);
}

#[test]
fn search_honours_the_limit() {
    let mut query = Query::new("gimp");
    query.limit = 2;
    let found = source().search(&query).unwrap();
    let names: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(names, ["gimp", "gimp-data"]);
}

#[test]
fn installed_is_every_status_entry_that_is_fully_installed() {
    let installed = source().installed().unwrap();
    let names: Vec<&str> = installed.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        names,
        [
            "brokey",
            "firefox-esr",
            "fonts-dejavu",
            "gimp",
            "gimp-data",
            "libgimp2.0",
            "old-tool"
        ]
    );
    assert!(
        installed
            .iter()
            .all(|p| p.installed && p.installed_version.is_some())
    );
    // The newest version across every list wins: firefox-esr from security.
    let firefox = installed.iter().find(|p| p.id == "firefox-esr").unwrap();
    assert_eq!(firefox.version.as_deref(), Some("115.8.0esr-1~deb12u1"));
    assert_eq!(firefox.repo.as_deref(), Some("bookworm-security/main"));
    assert_eq!(firefox.download_size, Some(68_301_122));
}

#[test]
fn updates_are_what_apt_would_upgrade_and_never_held_ones() {
    let updates = source().updates().unwrap();
    let rows: Vec<(&str, Option<&str>, &str)> = updates
        .iter()
        .map(|u| (u.name.as_str(), u.from.as_deref(), u.to.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("brokey", Some("0.1.0-1"), "0.2.0-1"),
            (
                "firefox-esr",
                Some("115.6.0esr-1~deb12u1"),
                "115.8.0esr-1~deb12u1"
            ),
            ("gimp", Some("2.10.34-1+deb12u1"), "2.10.34-1+deb12u2"),
        ],
        "libgimp2.0 is held and gimp-data is already newest"
    );
    assert!(updates[0].is_self);
    assert!(!updates[1].is_self);
    assert_eq!(updates[1].package, apt_ref("firefox-esr"));
    assert_eq!(updates[1].download_size, Some(68_301_122));
    assert_eq!(
        updates[1].summary.as_deref(),
        Some("Mozilla Firefox web browser - Extended Support Release (ESR)")
    );
    assert_eq!(updates[2].kind, PackageKind::Package);
    assert_eq!(updates[2].published, None);
}

#[test]
fn a_newer_version_apt_keeps_back_is_not_an_update() {
    // Pop!_OS pins its own repository above Ubuntu's, backports are never
    // upgraded to by themselves, and phased updates wait: in each the lists
    // hold a newer version and apt says it will not install it. Here apt
    // keeps firefox-esr back.
    let apt = source_simulating(
        "Inst brokey [0.1.0-1] (0.2.0-1 apt.example.org:bookworm [amd64])\n\
         Inst gimp [2.10.34-1+deb12u1] (2.10.34-1+deb12u2 Debian:12.5/stable [amd64])\n",
    );
    let names: Vec<String> = apt.updates().unwrap().into_iter().map(|u| u.name).collect();
    assert_eq!(names, ["brokey", "gimp"]);
    assert!(
        apt.plan(&Op::Update {
            package: apt_ref("firefox-esr")
        })
        .unwrap()
        .is_empty(),
        "an update apt would answer with \"is already the newest version\""
    );
    let all = apt
        .plan(&Op::UpdateAll {
            source: SourceKind::Apt,
        })
        .unwrap();
    assert_eq!(all[0].title, "Updating 2 apt packages");
    let firefox = apt.details("firefox-esr").unwrap();
    assert_eq!(
        firefox.version, firefox.installed_version,
        "no update on offer"
    );
}

#[test]
fn when_apt_cannot_say_what_it_would_upgrade_the_updates_say_so() {
    let apt = Apt::at(
        &debian(),
        catalogue(),
        fixtures().join("lists"),
        fixtures().join("status"),
        apt_get(),
    )
    .with_simulation(Box::new(|| {
        Err(brokey_core::Error::from_source(
            SourceKind::Apt,
            "apt could not say which updates it would install. apt-get upgrade failed: E: The package lists or status file could not be parsed or opened.",
        ))
    }));
    let err = apt.updates().unwrap_err();
    assert!(
        err.message.starts_with("apt could not say which updates"),
        "{}",
        err.message
    );
    // Search and details still work, showing the lists' version.
    assert_eq!(
        apt.details("gimp").unwrap().version.as_deref(),
        Some("2.10.34-1+deb12u2")
    );
}

#[test]
fn simulation_output_gives_upgrades_only() {
    let text = "\
Inst systemd [255.4-1ubuntu8.15pop0~1778766128~24.04~85b5073] (255.4-1ubuntu8.16 Ubuntu:24.04/noble-updates [amd64])
Inst linux-image-6.19.1-generic (6.19.1-76061901.202609 pop-os-release:24.04/noble [amd64])
Inst libc6:i386 [2.39-0ubuntu8.4] (2.39-0ubuntu8.5 Ubuntu:24.04/noble-updates [i386])
Inst libc6 [2.39-0ubuntu8.4] (2.39-0ubuntu8.5 Ubuntu:24.04/noble-updates [amd64])
Inst tzdata:all [2024a-3ubuntu1.1] (2024a-3ubuntu1.2 Ubuntu:24.04/noble-updates [all])
Conf systemd (255.4-1ubuntu8.16 Ubuntu:24.04/noble-updates [amd64])
3 upgraded, 1 newly installed, 0 to remove and 0 not upgraded.
";
    assert_eq!(
        parse_simulation(text, "amd64"),
        [
            ("systemd".to_string(), "255.4-1ubuntu8.16".to_string()),
            ("libc6".to_string(), "2.39-0ubuntu8.5".to_string()),
            ("tzdata".to_string(), "2024a-3ubuntu1.2".to_string()),
        ],
        "a new package and a foreign architecture are not upgrades here"
    );
}

#[test]
fn details_carry_the_description_and_the_facts() {
    let gimp = source().details("gimp").unwrap();
    assert_eq!(
        gimp.description.as_deref(),
        Some(
            "<p>GIMP is an advanced picture editor. You can use it to edit, enhance, and retouch photos and scans, \
             create drawings, and make your own images.</p><p>It has a large collection of professional-level \
             editing tools and filters, similar to the ones you might find in Photoshop.</p>"
        )
    );
    assert_eq!(
        gimp.facts,
        vec![
            ("Section".to_string(), "graphics".to_string()),
            (
                "Maintainer".to_string(),
                "Debian GNOME Maintainers <pkg-gnome-maintainers@lists.alioth.debian.org>".to_string()
            ),
            (
                "Depends".to_string(),
                "gimp-data (>= 2.10.34-1+deb12u2), libgimp2.0 (>= 2.10.34-1+deb12u2), libc6 (>= 2.34), \
                 libglib2.0-0 (>= 2.66), libgtk2.0-0 (>= 2.24.10)"
                    .to_string()
            ),
            ("Origin".to_string(), "deb.debian.org".to_string()),
        ]
    );
    let err = source().details("nonesuch").unwrap_err();
    assert_eq!(
        err.message,
        "nonesuch is not in any apt list on this machine and is not installed. Refresh the lists and search again."
    );
}

#[test]
fn plans_are_apt_get_as_root_with_a_quiet_frontend() {
    let apt = source();
    let install = apt
        .plan(&Op::Install {
            package: apt_ref("krita"),
        })
        .unwrap();
    assert_eq!(install.len(), 1);
    let step = &install[0];
    assert_eq!(step.source, SourceKind::Apt);
    assert_eq!(step.title, "Installing krita");
    assert_eq!(step.command.program, "apt-get");
    assert_eq!(step.command.args, ["install", "-y", "krita"]);
    assert_eq!(
        step.command.env,
        [
            ("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()),
            ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ]
    );
    assert_eq!(step.command.cwd, None);

    let remove = apt
        .plan(&Op::Remove {
            package: apt_ref("gimp"),
        })
        .unwrap();
    assert_eq!(remove[0].command.args, ["remove", "-y", "gimp"]);
    assert_eq!(remove[0].title, "Removing gimp");

    let update = apt
        .plan(&Op::Update {
            package: apt_ref("gimp"),
        })
        .unwrap();
    assert_eq!(
        update[0].command.args,
        ["install", "--only-upgrade", "-y", "gimp"]
    );
    assert_eq!(update[0].title, "Updating gimp");

    let all = apt
        .plan(&Op::UpdateAll {
            source: SourceKind::Apt,
        })
        .unwrap();
    assert_eq!(all[0].command.args, ["upgrade", "--with-new-pkgs", "-y"]);
    assert_eq!(all[0].title, "Updating 3 apt packages");
    assert!(
        all[0].weight > update[0].weight,
        "three packages weigh more than one"
    );

    let refresh = apt
        .plan(&Op::Refresh {
            source: SourceKind::Apt,
        })
        .unwrap();
    assert_eq!(refresh[0].command.args, ["update"]);
    assert_eq!(refresh[0].title, "Refreshing the apt package lists");
    assert!(refresh[0].weight < update[0].weight);

    for step in install
        .iter()
        .chain(&remove)
        .chain(&update)
        .chain(&all)
        .chain(&refresh)
    {
        assert!(step.needs_root, "{}", step.title);
        assert_eq!(step.command.program, "apt-get");
        assert!(
            !step.title.contains('\u{2014}'),
            "no em dashes: {}",
            step.title
        );
    }
}

#[test]
fn a_plan_with_nothing_to_do_is_empty_and_an_impossible_one_says_why() {
    let apt = source();
    assert!(
        apt.plan(&Op::Install {
            package: apt_ref("gimp")
        })
        .unwrap()
        .is_empty(),
        "already installed"
    );
    assert!(
        apt.plan(&Op::Remove {
            package: apt_ref("krita")
        })
        .unwrap()
        .is_empty(),
        "not installed"
    );
    assert!(
        apt.plan(&Op::Update {
            package: apt_ref("gimp-data")
        })
        .unwrap()
        .is_empty(),
        "already the newest"
    );

    let err = apt
        .plan(&Op::Install {
            package: apt_ref("nonesuch"),
        })
        .unwrap_err();
    assert_eq!(
        err.message,
        "nonesuch is not in any apt list on this machine. Refresh the lists and try again."
    );
    let err = apt
        .plan(&Op::Update {
            package: apt_ref("krita"),
        })
        .unwrap_err();
    assert_eq!(
        err.message,
        "krita is not installed, so there is nothing to update."
    );
    let flatpak = PackageRef {
        source: SourceKind::Flatpak,
        id: "org.gimp.GIMP".to_string(),
    };
    let err = apt.plan(&Op::Install { package: flatpak }).unwrap_err();
    assert_eq!(
        err.message,
        "org.gimp.GIMP is a Flatpak package, not an apt package."
    );

    // No lists at all: nothing is pending, so "Update all" has nothing to do.
    let dir = tempfile::tempdir().unwrap();
    let bare = Apt::at(
        &debian(),
        catalogue(),
        dir.path().join("lists"),
        fixtures().join("status"),
        apt_get(),
    );
    assert!(bare.status().available);
    assert_eq!(
        bare.status().detail.as_deref(),
        Some("no package lists yet")
    );
    assert!(bare.updates().unwrap().is_empty());
    assert!(
        bare.plan(&Op::UpdateAll {
            source: SourceKind::Apt
        })
        .unwrap()
        .is_empty()
    );
    assert_eq!(
        bare.installed().unwrap().len(),
        7,
        "the status file still says what is installed"
    );
}

#[test]
fn the_index_is_rebuilt_when_a_list_changes() {
    let dir = tempfile::tempdir().unwrap();
    let lists = dir.path().join("lists");
    std::fs::create_dir(&lists).unwrap();
    let main = "deb.debian.org_debian_dists_bookworm_main_binary-amd64_Packages";
    std::fs::copy(fixtures().join("lists").join(main), lists.join(main)).unwrap();
    let apt = Apt::at(
        &debian(),
        catalogue(),
        lists.clone(),
        fixtures().join("status"),
        apt_get(),
    );
    assert!(apt.search(&Query::new("newthing")).unwrap().is_empty());

    let mut text = std::fs::read_to_string(lists.join(main)).unwrap();
    text.push_str("\nPackage: newthing\nVersion: 1.0-1\nArchitecture: amd64\nDescription: a thing that just arrived\n");
    std::fs::write(lists.join(main), text).unwrap();
    let found = apt.search(&Query::new("newthing")).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].repo.as_deref(), Some("bookworm/main"));
    assert_eq!(
        found[0].summary.as_deref(),
        Some("a thing that just arrived")
    );
}

#[test]
fn a_gzipped_list_reads_the_same() {
    let dir = tempfile::tempdir().unwrap();
    let lists = dir.path().join("lists");
    std::fs::create_dir(&lists).unwrap();
    let text = std::fs::read(
        fixtures().join("lists/deb.debian.org_debian_dists_bookworm_main_binary-amd64_Packages"),
    )
    .unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&text).unwrap();
    let bytes = encoder.finish().unwrap();
    std::fs::write(
        lists.join("mirror.example.org_debian_dists_bookworm_main_binary-amd64_Packages.gz"),
        bytes,
    )
    .unwrap();
    let apt = Apt::at(
        &debian(),
        catalogue(),
        lists,
        fixtures().join("status"),
        apt_get(),
    );
    let found = apt.search(&Query::new("krita")).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].repo.as_deref(), Some("bookworm/main"));
    assert_eq!(found[0].version.as_deref(), Some("1:5.1.5+dfsg-1"));
}

#[test]
fn the_fixture_lists_parse_stanza_by_stanza() {
    let text = std::fs::read_to_string(
        fixtures().join("lists/deb.debian.org_debian_dists_bookworm_main_binary-amd64_Packages"),
    )
    .unwrap();
    let stanzas = parse_control(&text);
    let names: Vec<&str> = stanzas.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "gimp",
            "gimp-data",
            "libgimp2.0",
            "fonts-dejavu",
            "firefox-esr",
            "krita"
        ]
    );
    let arches: Vec<&str> = stanzas.iter().map(|s| s.architecture.as_str()).collect();
    assert_eq!(arches, ["amd64", "all", "amd64", "all", "amd64", "amd64"]);
    assert_eq!(
        stanzas[0].description.len(),
        2,
        "two paragraphs split by a lone full stop"
    );
    assert_eq!(stanzas[0].section.as_deref(), Some("graphics"));
    assert_eq!(stanzas[0].size, Some(5_204_492));
    assert_eq!(stanzas[0].installed_size, Some(20_260 * 1024));
    assert_eq!(stanzas[0].status, None);
    assert_eq!(
        stanzas[0].repo, None,
        "the repository comes from the file name, not the stanza"
    );

    let status = std::fs::read_to_string(fixtures().join("status")).unwrap();
    let entries = parse_control(&status);
    assert_eq!(entries.len(), 9);
    let installed: Vec<&str> = entries
        .iter()
        .filter(|e| e.is_installed())
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(
        installed,
        [
            "gimp",
            "gimp-data",
            "libgimp2.0",
            "fonts-dejavu",
            "firefox-esr",
            "old-tool",
            "brokey"
        ]
    );
    let held: Vec<&str> = entries
        .iter()
        .filter(|e| e.is_held())
        .map(|e| e.name.as_str())
        .collect();
    assert_eq!(held, ["libgimp2.0"]);
}

#[test]
fn list_names_decode_where_a_list_came_from() {
    let debian =
        ListName::parse("deb.debian.org_debian_dists_bookworm_main_binary-amd64_Packages").unwrap();
    assert_eq!(debian.host, "deb.debian.org");
    assert_eq!(debian.repo(), "bookworm/main");
    assert_eq!(debian.arch.as_deref(), Some("amd64"));
    assert!(debian.is_for("amd64"));
    assert!(!debian.is_for("arm64"));
    assert!(!debian.gzipped);

    let ubuntu = ListName::parse(
        "archive.ubuntu.com_ubuntu_dists_jammy-updates_universe_binary-arm64_Packages",
    )
    .unwrap();
    assert_eq!(ubuntu.repo(), "jammy-updates/universe");
    assert!(ubuntu.is_for("arm64"));

    let ppa = ListName::parse(
        "ppa.launchpadcontent.net_mozillateam_ppa_ubuntu_dists_jammy_main_binary-amd64_Packages",
    )
    .unwrap();
    assert_eq!(ppa.host, "ppa.launchpadcontent.net");
    assert_eq!(ppa.repo(), "jammy/main");

    // A flat repository ("deb https://dl.example.org/debian ./") has no
    // dists and no architecture in its name and serves every architecture.
    let flat = ListName::parse("dl.example.org_debian_._Packages").unwrap();
    assert_eq!(flat.repo(), "dl.example.org/debian");
    assert_eq!(flat.arch, None);
    assert!(flat.is_for("amd64"));

    let gz = ListName::parse("mirror.example.org_debian_dists_sid_main_binary-amd64_Packages.gz")
        .unwrap();
    assert!(gz.gzipped);
    assert_eq!(gz.repo(), "sid/main");

    for name in [
        "deb.debian.org_debian_dists_bookworm_InRelease",
        "deb.debian.org_debian_dists_bookworm_Release",
        "deb.debian.org_debian_dists_bookworm_main_i18n_Translation-en",
        "deb.debian.org_debian_dists_bookworm_main_dep11_Components-amd64.yml",
        "lock",
        "partial",
        "_Packages",
    ] {
        assert!(
            ListName::parse(name).is_none(),
            "{name} is not a Packages list"
        );
    }
}

#[test]
fn apt_get_output_becomes_a_step_message_without_a_fraction() {
    let cases = [
        (
            "Unpacking gimp (2.10.34-1+deb12u2) over (2.10.34-1+deb12u1) ...",
            Some("Unpacking gimp"),
        ),
        (
            "Setting up gimp (2.10.34-1+deb12u2) ...",
            Some("Setting up gimp"),
        ),
        (
            "Removing krita (1:5.1.5+dfsg-1) ...",
            Some("Removing krita"),
        ),
        (
            "Purging configuration files for krita (1:5.1.5+dfsg-1) ...",
            Some("Purging configuration files for krita"),
        ),
        (
            "Processing triggers for man-db (2.11.2-2) ...",
            Some("Processing triggers for man-db"),
        ),
        (
            "Preparing to unpack .../gimp_2.10.34-1+deb12u2_amd64.deb ...",
            Some("Preparing to unpack gimp"),
        ),
        (
            "Selecting previously unselected package krita.",
            Some("Selecting krita"),
        ),
        (
            "Get:1 http://deb.debian.org/debian bookworm/main amd64 gimp amd64 2.10.34-1+deb12u2 [5,083 kB]",
            Some("Downloading gimp"),
        ),
        (
            "Get:1 http://deb.debian.org/debian bookworm InRelease [151 kB]",
            Some("Downloading bookworm InRelease"),
        ),
        (
            "Fetched 5,083 kB in 2s (2,541 kB/s)",
            Some("Fetched 5,083 kB"),
        ),
        (
            "(Reading database ... 245678 files and directories currently installed.)",
            Some("Reading the package database"),
        ),
        (
            "Reading package lists... Done",
            Some("Reading package lists"),
        ),
        (
            "Building dependency tree... Done",
            Some("Building dependency tree"),
        ),
        ("", None),
        ("The following NEW packages will be installed:", None),
        ("  krita krita-data", None),
    ];
    for (line, want) in cases {
        assert_eq!(parse_progress(line).as_deref(), want, "{line:?}");
    }
}
