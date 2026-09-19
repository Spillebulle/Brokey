//! The pacman source against a tiny local database and four tiny sync
//! databases assembled from `fixtures/pacman/`, so every rule is exercised
//! without this machine's pacman. The `live_*` tests read the real
//! databases and, for the refresh, the real mirrors.

#![cfg(unix)]

use brokey_core::appstream::{Catalogue, Component};
use brokey_core::sources::linux::pacman::{Pacman, Paths, SKIPPED_FOR_TIME, mirrors};
use brokey_core::{Op, PackageKind, PackageRef, Picture, Query, Screenshot, Source, SourceKind};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pacman")
}

/// A sync database from a directory of `<name>-<version>/desc` files: a
/// gzip-compressed tar, one of the two compressions pacman writes
/// (`alpmdb` has its own test for the zstd one and for a plain tar).
fn sync_db(dir: &Path) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut packages: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    packages.sort();
    for package in packages {
        let desc = fs::read(package.join("desc")).unwrap();
        let name = package.file_name().unwrap().to_str().unwrap().to_string();
        let mut header = tar::Header::new_gnu();
        header.set_size(desc.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("{name}/desc"), desc.as_slice())
            .unwrap();
    }
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn set_mtime(path: &Path, t: SystemTime) {
    // A directory cannot be opened for writing; a read-only handle is
    // enough for futimens when the caller owns it, which a test does.
    let file = if path.is_dir() {
        fs::File::open(path).unwrap()
    } else {
        fs::File::options().write(true).open(path).unwrap()
    };
    file.set_modified(t).unwrap();
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

/// A pretend machine in a temporary directory: the fixture's pacman.conf
/// with its Include lines pointing at the fixture mirrorlists, the local
/// database copied so a test may add to it, and the four sync databases.
struct Machine {
    _dir: tempfile::TempDir,
    root: PathBuf,
    paths: Paths,
}

fn machine() -> Machine {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let fixture = fixtures();
    let conf = fs::read_to_string(fixture.join("pacman.conf"))
        .unwrap()
        .replace("@DIR@", fixture.to_str().unwrap());
    fs::write(root.join("pacman.conf"), conf).unwrap();
    copy_dir(&fixture.join("local"), &root.join("local"));
    fs::create_dir_all(root.join("sync")).unwrap();
    for repo in ["cachyos-v3", "core", "extra", "multilib"] {
        fs::write(
            root.join("sync").join(format!("{repo}.db")),
            sync_db(&fixture.join("sync").join(repo)),
        )
        .unwrap();
    }
    let paths = Paths {
        conf: root.join("pacman.conf"),
        sync_dir: root.join("sync"),
        local_dir: root.join("local"),
        cache_dir: root.join("cache"),
        pacman: Some(std::env::current_exe().unwrap()),
    };
    Machine {
        _dir: dir,
        root,
        paths,
    }
}

fn catalogue() -> Arc<Catalogue> {
    Arc::new(Catalogue::from_components(vec![
        Component {
            id: "com.valvesoftware.Steam".into(),
            origin: "archlinux-arch-multilib".into(),
            pkgname: Some("steam".into()),
            name: "Steam".into(),
            summary: Some("Launcher for the Steam software distribution service".into()),
            description: Some(
                "<p>Steam is a software distribution service with an online store.</p>".into(),
            ),
            developer: Some("Valve Corporation".into()),
            homepage: Some("https://store.steampowered.com/".into()),
            categories: vec!["Game".into(), "PackageManager".into()],
            keywords: vec!["games".into()],
            icon: Some(Picture::File(
                "/usr/share/swcatalog/icons/archlinux-arch-multilib/128x128/steam_steam.png".into(),
            )),
            screenshots: vec![Screenshot {
                image: Picture::Url("https://example.org/steam.png".into()),
                thumbnail: None,
                caption: None,
                width: Some(1280),
                height: Some(720),
            }],
            is_app: true,
            ..Default::default()
        },
        Component {
            id: "org.gimp.GIMP".into(),
            origin: "archlinux-arch-extra".into(),
            pkgname: Some("gimp".into()),
            name: "GNU Image Manipulation Program".into(),
            summary: Some("Create images and edit photographs".into()),
            keywords: vec!["photo".into(), "editor".into()],
            is_app: true,
            ..Default::default()
        },
        // firefox carries two desktop entries, the safe-mode one first in
        // the catalogue; the package is named by the one whose id ends in
        // its name.
        Component {
            id: "firefox-safe-mode".into(),
            origin: "archlinux-arch-extra".into(),
            pkgname: Some("firefox".into()),
            name: "Firefox (Safe Mode)".into(),
            is_app: true,
            component_type: "desktop-application".into(),
            ..Default::default()
        },
        Component {
            id: "org.mozilla.firefox".into(),
            origin: "archlinux-arch-extra".into(),
            pkgname: Some("firefox".into()),
            name: "Firefox".into(),
            summary: Some("Fast, private and secure web browser".into()),
            is_app: true,
            component_type: "desktop-application".into(),
            ..Default::default()
        },
    ]))
}

fn pacman(m: &Machine) -> Pacman {
    Pacman::with_paths(m.paths.clone(), catalogue())
}

fn reference(id: &str) -> PackageRef {
    PackageRef {
        source: SourceKind::Pacman,
        id: id.to_string(),
    }
}

#[test]
fn status_is_available_and_lists_the_synced_repositories_in_order() {
    let m = machine();
    let s = pacman(&m).status();
    assert!(s.available, "{:?}", s.reason);
    assert_eq!(s.kind, SourceKind::Pacman);
    // never-synced is enabled in pacman.conf but has no database yet.
    assert_eq!(
        s.detail.as_deref(),
        Some("cachyos-v3, core, extra, multilib")
    );

    let no_local = Paths {
        local_dir: m.root.join("nowhere"),
        ..m.paths.clone()
    };
    let s = Pacman::with_paths(no_local, catalogue()).status();
    assert!(!s.available);
    assert!(
        s.reason
            .unwrap()
            .ends_with("is missing, so the installed packages cannot be read.")
    );
}

#[test]
fn search_scores_the_name_first_and_lists_a_name_once() {
    let m = machine();
    let p = pacman(&m);
    let found = p.search(&Query::new("steam")).unwrap();
    let ids: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["steam", "steam-native-runtime", "proton-cachyos"]);

    let steam = &found[0];
    assert_eq!(steam.source, SourceKind::Pacman);
    assert_eq!(steam.name, "Steam");
    assert_eq!(steam.kind, PackageKind::App);
    assert_eq!(steam.repo.as_deref(), Some("multilib"));
    assert_eq!(steam.version.as_deref(), Some("1.0.0.87-3"));
    assert!(steam.installed);
    assert_eq!(steam.installed_version.as_deref(), Some("1.0.0.86-2"));
    assert_eq!(
        steam.summary.as_deref(),
        Some("Launcher for the Steam software distribution service")
    );
    assert_eq!(steam.developer.as_deref(), Some("Valve Corporation"));
    assert_eq!(
        steam.licence.as_deref(),
        Some("LicenseRef-steam-subscriber-agreement")
    );
    assert_eq!(steam.homepage.as_deref(), Some("https://steampowered.com/"));
    assert_eq!(steam.updated, Some(1_785_004_799));
    assert_eq!(steam.download_size, Some(20_371_240));
    assert_eq!(steam.installed_size, Some(20_475_439));
    assert!(matches!(steam.icon, Some(Picture::File(_))));
    assert_eq!(steam.categories, ["Game", "PackageManager"]);
    assert_eq!(
        steam.appstream_id.as_deref(),
        Some("com.valvesoftware.Steam")
    );
    assert_eq!(steam.popularity, None);
    assert!(!steam.sandboxed);
    let fact = |k: &str| {
        steam
            .facts
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        fact("Packager"),
        Some("Levente Polyak <anthraxx@archlinux.org>")
    );
    assert_eq!(fact("Build date"), Some("2026-07-25"));
    assert_eq!(fact("Depends"), Some("2 packages"));
    assert_eq!(fact("Architecture"), Some("x86_64"));

    // No component: the package's own name and description, kind Package.
    let runtime = &found[1];
    assert_eq!(runtime.name, "steam-native-runtime");
    assert_eq!(runtime.kind, PackageKind::Package);
    assert_eq!(
        runtime.summary.as_deref(),
        Some("Native replacement for the Steam runtime using system libraries")
    );
    assert!(!runtime.installed);
    assert!(runtime.icon.is_none());

    // firefox is in cachyos-v3 and extra; pacman's rule is the first wins.
    let firefox = p.search(&Query::new("Firefox")).unwrap();
    assert_eq!(firefox.len(), 1);
    assert_eq!(firefox[0].repo.as_deref(), Some("cachyos-v3"));
    assert_eq!(firefox[0].version.as_deref(), Some("130.0-2"));
    // Of its two desktop entries the catalogue's ranked choice names it,
    // not the one listed first.
    assert_eq!(firefox[0].name, "Firefox");
    assert_eq!(
        firefox[0].appstream_id.as_deref(),
        Some("org.mozilla.firefox")
    );
    assert_eq!(firefox[0].kind, PackageKind::App);

    // A component's name and keywords count, so the launcher's words work.
    let gimp = p.search(&Query::new("image manipulation")).unwrap();
    assert_eq!(gimp[0].id, "gimp");
    assert_eq!(gimp[0].name, "GNU Image Manipulation Program");
    let photo = p.search(&Query::new("photo")).unwrap();
    assert_eq!(
        photo.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["gimp"]
    );

    // Groups and provides come through as facts; fonts are told by name.
    let bash = p.search(&Query::new("bash")).unwrap();
    assert!(
        bash[0]
            .facts
            .contains(&("Groups".to_string(), "base-devel".to_string()))
    );
    assert!(
        bash[0]
            .facts
            .contains(&("Provides".to_string(), "sh".to_string()))
    );
    assert!(
        bash[0]
            .facts
            .contains(&("Install date".to_string(), "2026-08-29".to_string()))
    );
    assert_eq!(bash[0].licence.as_deref(), Some("GPL-3.0-or-later"));
    let dejavu = p.search(&Query::new("dejavu")).unwrap();
    assert_eq!(dejavu[0].kind, PackageKind::Font);
    let coreutils = p.search(&Query::new("coreutils")).unwrap();
    assert_eq!(
        coreutils[0].licence.as_deref(),
        Some("GPL-3.0-or-later, LGPL-3.0-or-later")
    );

    assert!(p.search(&Query::new("")).unwrap().is_empty());
    assert!(p.search(&Query::new("   ")).unwrap().is_empty());
    assert!(
        p.search(&Query::new("nothing-like-this"))
            .unwrap()
            .is_empty()
    );
    let mut one = Query::new("steam");
    one.limit = 1;
    assert_eq!(p.search(&one).unwrap().len(), 1);
}

#[test]
fn installed_lists_repository_packages_and_foreign_ones_separately() {
    let m = machine();
    let p = pacman(&m);
    let installed = p.installed().unwrap();
    let ids: Vec<&str> = installed.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["bash", "brokey", "linux-cachyos", "steam"]);
    assert!(installed.iter().all(|p| p.installed));
    assert_eq!(
        installed[3].installed_version.as_deref(),
        Some("1.0.0.86-2")
    );

    let foreign = p.foreign_packages();
    assert_eq!(foreign.len(), 1);
    let paru = &foreign[0];
    assert_eq!(paru.name, "paru");
    assert_eq!(paru.version, "2.1.0-1");
    assert_eq!(paru.base.as_deref(), Some("paru"));
    assert_eq!(
        paru.description.as_deref(),
        Some("Feature packed AUR helper")
    );
    assert_eq!(
        paru.url.as_deref(),
        Some("https://github.com/morganamilo/paru")
    );
    assert_eq!(paru.installed_at, Some(1_786_500_000));
    assert_eq!(paru.installed_size, Some(7_500_000));

    assert!(p.is_repo_package("steam"));
    assert!(p.is_repo_package("coreutils"));
    assert!(!p.is_repo_package("paru"));
    assert!(!p.is_repo_package("nothing"));
}

#[test]
fn updates_compare_by_vercmp_and_honour_ignorepkg() {
    let m = machine();
    let p = pacman(&m);
    let updates = p.updates().unwrap();
    let ids: Vec<&str> = updates.iter().map(|u| u.package.id.as_str()).collect();
    // bash is current; linux-cachyos is behind but in IgnorePkg.
    assert_eq!(ids, ["brokey", "steam"]);

    // pacman.conf allows shell globs in both: `nvidia*` is the common
    // shape on an NVIDIA machine, and what pacman -Syu skips must not be
    // listed as an update.
    let conf = fs::read_to_string(&m.paths.conf).unwrap();
    let ignoring = |ignore: &str| {
        fs::write(
            &m.paths.conf,
            conf.replace("IgnorePkg   = linux-cachyos", ignore),
        )
        .unwrap();
        let updates = pacman(&m).updates().unwrap();
        updates
            .iter()
            .map(|u| u.package.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ignoring("IgnorePkg = nvidia* brok*"),
        ["linux-cachyos", "steam"]
    );
    assert_eq!(
        ignoring("IgnorePkg = nvidia*\nIgnoreGroup = gam*"),
        ["brokey", "linux-cachyos"],
        "steam is in the games group"
    );
    assert_eq!(
        ignoring("IgnorePkg = nvidia*\nIgnoreGroup = base-devel"),
        ["brokey", "linux-cachyos", "steam"],
        "a glob that matches nothing here hides nothing"
    );

    let store = &updates[0];
    assert!(store.is_self);
    assert_eq!(store.from.as_deref(), Some("0.1.0-1"));
    assert_eq!(store.to, "0.2.0-1");
    assert_eq!(store.kind, PackageKind::Package);
    assert_eq!(store.download_size, Some(6_000_000));
    assert_eq!(store.published, Some(1_788_700_000));

    let steam = &updates[1];
    assert!(!steam.is_self);
    assert_eq!(steam.package.source, SourceKind::Pacman);
    assert_eq!(steam.name, "Steam");
    assert_eq!(steam.kind, PackageKind::App);
    assert_eq!(steam.from.as_deref(), Some("1.0.0.86-2"));
    assert_eq!(steam.to, "1.0.0.87-3");
    assert_eq!(steam.download_size, Some(20_371_240));
    assert!(matches!(steam.icon, Some(Picture::File(_))));
}

#[test]
fn updates_prefer_a_cached_database_only_while_it_is_newer() {
    let m = machine();
    let p = pacman(&m);
    let system_db = m.root.join("sync/multilib.db");
    let cached_db = m.root.join("cache/multilib.db");
    fs::create_dir_all(m.root.join("cache")).unwrap();
    fs::write(&cached_db, sync_db(&fixtures().join("sync-newer/multilib"))).unwrap();

    // Newer than the system's: updates see it, search and installed do not.
    set_mtime(&cached_db, mtime(&system_db) + Duration::from_secs(60));
    let updates = p.updates().unwrap();
    let steam = updates.iter().find(|u| u.package.id == "steam").unwrap();
    assert_eq!(steam.to, "1.0.0.88-1");
    assert_eq!(steam.download_size, Some(20_500_000));
    let search = p.search(&Query::new("steam")).unwrap();
    assert_eq!(search[0].version.as_deref(), Some("1.0.0.87-3"));
    assert_eq!(
        p.details("steam").unwrap().version.as_deref(),
        Some("1.0.0.87-3")
    );

    // Older than the system's (the machine ran pacman -Sy since): ignored.
    set_mtime(&cached_db, mtime(&system_db) - Duration::from_secs(60));
    let updates = p.updates().unwrap();
    let steam = updates.iter().find(|u| u.package.id == "steam").unwrap();
    assert_eq!(steam.to, "1.0.0.87-3");

    // A corrupt cached copy is ignored and removed, so the next refresh
    // does not ask "modified since" against garbage.
    fs::write(&cached_db, b"<html>not a database</html>").unwrap();
    set_mtime(&cached_db, mtime(&system_db) + Duration::from_secs(120));
    let updates = p.updates().unwrap();
    assert_eq!(
        updates.iter().find(|u| u.package.id == "steam").unwrap().to,
        "1.0.0.87-3"
    );
    assert!(!cached_db.exists());
}

#[test]
fn a_change_on_disk_is_noticed_on_the_next_call() {
    let m = machine();
    let p = pacman(&m);
    assert_eq!(p.installed().unwrap().len(), 4);

    // Something else installs coreutils: a new directory in the local db.
    let new_dir = m.root.join("local/coreutils-9.5-1");
    fs::create_dir_all(&new_dir).unwrap();
    fs::copy(
        fixtures().join("sync/core/coreutils-9.5-1/desc"),
        new_dir.join("desc"),
    )
    .unwrap();
    set_mtime(
        &m.root.join("local"),
        SystemTime::now() + Duration::from_secs(60),
    );
    let installed = p.installed().unwrap();
    assert_eq!(installed.len(), 5);
    assert!(installed.iter().any(|p| p.id == "coreutils"));

    // Something else removes a repository's database.
    fs::remove_file(m.root.join("sync/multilib.db")).unwrap();
    assert_eq!(
        p.status().detail.as_deref(),
        Some("cachyos-v3, core, extra")
    );
    let found = p.search(&Query::new("steam")).unwrap();
    assert_eq!(
        found.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        ["steam-native-runtime"]
    );
    assert_eq!(p.installed().unwrap().len(), 4);
    assert!(p.foreign_packages().iter().any(|f| f.name == "steam"));
}

#[test]
fn details_fill_the_description_and_the_whole_dependency_list() {
    let m = machine();
    let p = pacman(&m);
    let steam = p.details("steam").unwrap();
    assert_eq!(steam.name, "Steam");
    assert_eq!(
        steam.description.as_deref(),
        Some("<p>Steam is a software distribution service with an online store.</p>")
    );
    assert_eq!(steam.screenshots.len(), 1);
    assert_eq!(steam.screenshots[0].width, Some(1280));
    assert!(
        steam
            .facts
            .contains(&("Depends".to_string(), "bash, coreutils".to_string()))
    );
    assert!(
        steam
            .facts
            .contains(&("Install date".to_string(), "2026-09-09".to_string()))
    );

    let bash = p.details("bash").unwrap();
    assert_eq!(bash.description, None);
    assert!(
        bash.facts
            .contains(&("Depends".to_string(), "readline, glibc".to_string()))
    );

    let e = p.details("paru").unwrap_err();
    assert_eq!(e.message, "paru is not in any enabled repository.");
    assert_eq!(e.source_kind, Some(SourceKind::Pacman));
}

#[test]
fn a_plan_is_one_pacman_step_per_operation() {
    let m = machine();
    let p = pacman(&m);
    let steps = p
        .plan(&Op::Install {
            package: reference("steam"),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].source, SourceKind::Pacman);
    assert_eq!(steps[0].command.program, "pacman");
    // The system's databases are days old and the mirrors have moved on,
    // so an install refreshes and upgrades in the one transaction.
    assert_eq!(
        steps[0].command.args,
        ["-Syu", "--noconfirm", "--needed", "steam"]
    );
    assert_eq!(steps[0].title, "Installing steam and updating the system");
    assert!(steps[0].needs_root);
    assert!(
        steps[0]
            .command
            .env
            .contains(&("LC_ALL".to_string(), "C.UTF-8".to_string()))
    );
    let update = p
        .plan(&Op::Update {
            package: reference("steam"),
        })
        .unwrap();
    assert_eq!(
        update[0].command.args,
        ["-Syu", "--noconfirm", "--needed", "steam"]
    );
    assert_eq!(update[0].title, "Updating steam and the system");
    let all = p
        .plan(&Op::UpdateAll {
            source: SourceKind::Pacman,
        })
        .unwrap();
    assert_eq!(all[0].command.args, ["-Syu", "--noconfirm"]);
    assert_eq!(all[0].title, "Updating the system");
    // A refresh is done without root by refresh_index; a root -Sy on its
    // own would leave the system set up for a partial upgrade.
    assert!(
        p.plan(&Op::Refresh {
            source: SourceKind::Pacman,
        })
        .unwrap()
        .is_empty()
    );
}

#[test]
fn mirrors_come_from_the_included_lists_with_the_arch_substituted() {
    let m = machine();
    let conf = fs::read_to_string(&m.paths.conf).unwrap();
    let read = |path: &str| fs::read_to_string(path).into_iter().collect::<Vec<_>>();
    let list = mirrors(&conf, "x86_64", &read);
    let names: Vec<&str> = list.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        ["cachyos-v3", "core", "extra", "multilib", "never-synced"]
    );
    assert_eq!(
        list[0].servers,
        [
            "https://mirror.example.org/cachyos/repo/x86_64_v3/cachyos-v3",
            "https://cdn.example.org/repo/x86_64_v3/cachyos-v3"
        ]
    );
    assert_eq!(
        list[1].servers,
        [
            "https://mirror.example.org/archlinux/core/os/x86_64",
            "https://second.example.org/core/os/x86_64"
        ]
    );
    assert_eq!(
        list[3].servers[0],
        "https://mirror.example.org/archlinux/multilib/os/x86_64"
    );
    assert_eq!(
        list[4].servers,
        ["https://example.invalid/never-synced/os/x86_64"]
    );
}

#[test]
fn a_refresh_that_cannot_reach_a_mirror_says_so_per_repository() {
    let m = machine();
    // Port 9 on loopback answers nothing: the connection is refused at
    // once, and no packet leaves the machine.
    fs::write(
        &m.paths.conf,
        "[options]\nArchitecture = x86_64\n[core]\nServer = http://127.0.0.1:9/$repo/os/$arch\n[extra]\nServer = http://127.0.0.1:9/$repo/os/$arch\n[empty]\n",
    )
    .unwrap();
    let p = pacman(&m);
    let client = brokey_core::http::Client::new(m.root.join("http"));
    let r = p
        .refresh_with_deadline(&client, Instant::now() + Duration::from_secs(20))
        .unwrap();
    assert!(r.downloaded.is_empty());
    assert!(r.unchanged.is_empty());
    let names: Vec<&str> = r.failed.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["core", "extra", "empty"]);
    for (name, why) in &r.failed {
        assert!(why.ends_with('.'), "{name}: {why}");
        assert!(!why.contains('\u{2014}'), "{name}: {why}");
    }
    assert!(
        r.failed[0].1.starts_with("Could not reach 127.0.0.1:9"),
        "{}",
        r.failed[0].1
    );
    assert_eq!(
        r.failed[2].1,
        "No Server line in pacman.conf or its mirrorlist."
    );
    assert!(m.root.join("cache").is_dir());
    assert_eq!(fs::read_dir(m.root.join("cache")).unwrap().count(), 0);

    // Nothing fresh at all is an error the caller reports, in one sentence
    // that names each reason once with the repositories it took down.
    let e = p.refresh_into_cache(&client).unwrap_err();
    assert_eq!(e.source_kind, Some(SourceKind::Pacman));
    assert_eq!(
        e.message,
        "No package list could be refreshed, so updates are checked against the lists the machine has. Could not reach 127.0.0.1:9 (core, extra). No Server line in pacman.conf or its mirrorlist (empty)."
    );
    assert_eq!(
        p.refresh_index().unwrap_err().message,
        e.message,
        "the Source method carries the same sentence"
    );
}

#[test]
fn a_refresh_past_its_deadline_asks_no_mirror_and_names_the_repositories_it_skipped() {
    let m = machine();
    fs::write(
        &m.paths.conf,
        "[options]\nArchitecture = x86_64\n[core]\nServer = http://127.0.0.1:9/$repo/os/$arch\n[extra]\nServer = http://127.0.0.1:9/$repo/os/$arch\n[empty]\n",
    )
    .unwrap();
    let p = pacman(&m);
    let client = brokey_core::http::Client::new(m.root.join("http"));
    // The budget was used up before these repositories could start: no
    // mirror is asked (a request would have failed with "Could not reach"),
    // and each is listed with the sentence that says so.
    let r = p
        .refresh_with_deadline(&client, Instant::now() - Duration::from_secs(1))
        .unwrap();
    assert!(r.downloaded.is_empty() && r.unchanged.is_empty());
    assert_eq!(
        r.failed,
        [
            ("core".to_string(), SKIPPED_FOR_TIME.to_string()),
            ("extra".to_string(), SKIPPED_FOR_TIME.to_string()),
            (
                "empty".to_string(),
                "No Server line in pacman.conf or its mirrorlist.".to_string()
            ),
        ]
    );
    assert_eq!(
        SKIPPED_FOR_TIME,
        "Not refreshed: the earlier repositories used the time budget."
    );
    let sentence = r.failure().unwrap();
    assert!(
        sentence
            .contains("Not refreshed: the earlier repositories used the time budget (core, extra)"),
        "{sentence}"
    );
    // With time left the same repositories are asked, and fail for real.
    let r = p
        .refresh_with_deadline(&client, Instant::now() + Duration::from_secs(20))
        .unwrap();
    assert!(
        r.failed[0].1.starts_with("Could not reach"),
        "{}",
        r.failed[0].1
    );
    assert_eq!(fs::read_dir(m.root.join("cache")).unwrap().count(), 0);
}

// The live tests read this machine's databases. The refresh downloads
// from the configured mirrors into a temporary cache, never into the
// system's directory.

fn live() -> (tempfile::TempDir, Pacman) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        cache_dir: dir.path().join("pacman-sync"),
        ..Paths::system()
    };
    let p = Pacman::with_paths(paths, Arc::new(Catalogue::default()));
    let status = p.status();
    assert!(status.available, "{:?}", status.reason);
    println!("pacman: {}", status.detail.unwrap_or_default());
    (dir, p)
}

#[test]
#[ignore = "reads this machine's pacman databases"]
fn live_search_finds_steam_in_multilib() {
    let (_dir, p) = live();
    let started = Instant::now();
    let found = p.search(&Query::new("steam")).unwrap();
    println!(
        "search took {:?} (includes the first load)",
        started.elapsed()
    );
    let steam = found
        .iter()
        .find(|p| p.id == "steam")
        .expect("steam is in multilib");
    assert_eq!(found[0].id, "steam", "the exact name comes first");
    assert_eq!(steam.repo.as_deref(), Some("multilib"));
    assert_eq!(steam.version.as_deref(), Some("1.0.0.87-3"));
    assert!(steam.installed);
    assert_eq!(steam.installed_version.as_deref(), Some("1.0.0.87-3"));
    let again = Instant::now();
    let _ = p.search(&Query::new("browser")).unwrap();
    println!("a second search took {:?}", again.elapsed());
}

#[test]
#[ignore = "reads this machine's pacman databases"]
fn live_installed_has_hundreds_of_packages() {
    let (_dir, p) = live();
    let installed = p.installed().unwrap();
    println!("{} installed from the repositories", installed.len());
    assert!(installed.len() > 500, "only {}", installed.len());
    assert!(installed.iter().all(|p| p.installed && p.repo.is_some()));
}

#[test]
#[ignore = "reads this machine's pacman databases"]
fn live_updates_run() {
    let (_dir, p) = live();
    let updates = p.updates().unwrap();
    println!("{} updates against the system's lists", updates.len());
    for u in updates.iter().take(10) {
        println!(
            "  {} {} -> {}",
            u.package.id,
            u.from.as_deref().unwrap_or("?"),
            u.to
        );
    }
}

#[test]
#[ignore = "reads this machine's pacman databases and runs pacman -Qm"]
fn live_foreign_packages_match_pacman_qm() {
    let (_dir, p) = live();
    let foreign = p.foreign_packages();
    let ours: Vec<&str> = foreign.iter().map(|f| f.name.as_str()).collect();
    println!("foreign: {}", ours.join(", "));
    let out = std::process::Command::new("pacman")
        .args(["-Qm"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let mut theirs: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    theirs.sort();
    assert_eq!(ours, theirs);
    for name in &ours {
        assert!(!p.is_repo_package(name));
    }
    // paru is foreign on plain Arch and a repository package on CachyOS,
    // which ships it in [cachyos]; it is one or the other, never both.
    let paru_from_repo = p.installed().unwrap().iter().any(|x| x.id == "paru");
    let paru_foreign = ours.contains(&"paru");
    assert!(!(paru_from_repo && paru_foreign));
    println!(
        "paru is {}",
        if paru_foreign {
            "foreign"
        } else if paru_from_repo {
            "from a repository"
        } else {
            "not installed"
        }
    );
}

#[test]
#[ignore = "downloads every repository's database from its mirrors"]
fn live_refresh_downloads_into_the_cache_and_updates_use_it() {
    let (dir, p) = live();
    let client = brokey_core::http::Client::new(dir.path().join("http"));
    let started = Instant::now();
    let first = p.refresh_into_cache(&client).unwrap();
    println!(
        "first refresh in {:?}: downloaded {:?}, unchanged {:?}, failed {:?}",
        started.elapsed(),
        first.downloaded,
        first.unchanged,
        first.failed
    );
    assert!(!first.downloaded.is_empty() || !first.unchanged.is_empty());
    for repo in &first.downloaded {
        let path = p.paths().cache_dir.join(format!("{repo}.db"));
        assert!(path.is_file(), "{} was not written", path.display());
        println!(
            "  {repo}.db: {} bytes, mtime {:?}",
            fs::metadata(&path).unwrap().len(),
            mtime(&path)
        );
    }
    let updates = p.updates().unwrap();
    println!("{} updates against the refreshed lists", updates.len());
    for u in updates.iter().take(10) {
        println!(
            "  {} {} -> {}",
            u.package.id,
            u.from.as_deref().unwrap_or("?"),
            u.to
        );
    }
    // Asked again straight away, every mirror should answer "not modified".
    let again = Instant::now();
    let second = p.refresh_into_cache(&client).unwrap();
    println!(
        "second refresh in {:?}: downloaded {:?}, unchanged {:?}, failed {:?}",
        again.elapsed(),
        second.downloaded,
        second.unchanged,
        second.failed
    );
    assert!(!second.unchanged.is_empty());
}

#[test]
#[ignore = "times the load of this machine's pacman databases"]
fn live_load_is_fast() {
    let (_dir, p) = live();
    let started = Instant::now();
    let installed = p.installed().unwrap();
    let took = started.elapsed();
    println!(
        "loaded every database ({} installed) in {took:?}",
        installed.len()
    );
    assert!(took < Duration::from_secs(3), "{took:?}");
    let warm = Instant::now();
    let _ = p.installed().unwrap();
    println!("a warm call took {:?}", warm.elapsed());
}

#[test]
fn an_installed_package_with_a_listed_desktop_entry_is_an_application_without_a_catalogue() {
    let m = machine();
    let files_root = m.root.join("files-root");
    let write = |path: PathBuf, text: &str| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    // brokey ships an entry and an icon; linux-cachyos ships only a hidden entry; bash ships none.
    write(
        m.root.join("local/brokey-0.1.0-1/files"),
        "%FILES%\nusr/\nusr/bin/brokey\nusr/share/applications/io.github.spillebulle.brokey.desktop\n",
    );
    write(
        files_root.join("usr/share/applications/io.github.spillebulle.brokey.desktop"),
        "[Desktop Entry]\nType=Application\nName=Brokey\nComment=Find and install software\nIcon=io.github.spillebulle.brokey\nCategories=System;PackageManager;\n",
    );
    write(
        files_root.join("usr/share/icons/hicolor/256x256/apps/io.github.spillebulle.brokey.png"),
        "png",
    );
    write(
        m.root.join("local/linux-cachyos-6.16.5-1/files"),
        "%FILES%\nusr/share/applications/kernel-settings.desktop\n",
    );
    write(
        files_root.join("usr/share/applications/kernel-settings.desktop"),
        "[Desktop Entry]\nType=Application\nName=Kernel\nNoDisplay=true\n",
    );

    let p = Pacman::with_paths(
        m.paths.clone(),
        Arc::new(Catalogue::from_components(Vec::new())),
    )
    .with_root(files_root.clone());
    let installed = p.installed().unwrap();
    let by_id = |id: &str| installed.iter().find(|p| p.id == id).unwrap();

    let brokey = by_id("brokey");
    assert_eq!(brokey.kind, PackageKind::App);
    assert_eq!(brokey.name, "Brokey", "the launcher's name");
    assert_eq!(
        brokey.icon,
        Some(Picture::File(files_root.join(
            "usr/share/icons/hicolor/256x256/apps/io.github.spillebulle.brokey.png"
        )))
    );
    assert_eq!(brokey.categories, ["System", "PackageManager"]);
    assert_ne!(
        by_id("linux-cachyos").kind,
        PackageKind::App,
        "a hidden entry is not an application"
    );
    assert_ne!(
        by_id("bash").kind,
        PackageKind::App,
        "no entry, no application"
    );
}
