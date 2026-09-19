//! The GitHub source against answers captured from the public API
//! (`fixtures/github/`): a search for "heroic", Heroic's latest release
//! (every Linux package kind plus macOS and Windows noise) and neovim's
//! (AppImages for both architectures). One live search at the end.
//!
//! The source itself is Linux only for now (`sources::linux::github`).

#![cfg(unix)]

use brokey_core::appstream::Catalogue;
use brokey_core::http::Client;
use brokey_core::sources::linux::github::{
    self, Arch, AssetKind, Github, InstallRecord, InstallTarget,
};
use brokey_core::system::from_os_release;
use brokey_core::{Op, PackageKind, PackageRef, Picture, Query, Source, SourceKind, SystemInfo};
use std::path::Path;
use std::sync::Arc;

const SEARCH: &str = include_str!("fixtures/github/search.json");
const HEROIC: &str = include_str!("fixtures/github/release-heroic.json");
const NEOVIM: &str = include_str!("fixtures/github/release-neovim.json");

const HEROIC_ID: &str = "Heroic-Games-Launcher/HeroicGamesLauncher";

fn system(os_release: &str) -> SystemInfo {
    let mut s = from_os_release(os_release);
    s.arch = "x86_64".to_string();
    s
}

fn source(system: &SystemInfo, dir: &Path) -> Github {
    let client = Arc::new(Client::new(dir.join("http")));
    Github::with_dirs(
        system,
        client,
        Arc::new(Catalogue::default()),
        &dir.join("data"),
        &dir.join("cache"),
    )
}

fn fact<'a>(facts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    facts
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn heroic_record(kind: AssetKind) -> InstallRecord {
    InstallRecord {
        repo: HEROIC_ID.to_string(),
        name: "HeroicGamesLauncher".to_string(),
        asset: "Heroic-2.22.1-linux-x86_64.AppImage".to_string(),
        version: "2.22.1".to_string(),
        kind,
    }
}

#[test]
fn the_search_answer_parses() {
    let repos = github::parse_search(SEARCH).unwrap();
    assert_eq!(repos.len(), 4);
    let heroic = &repos[1];
    assert_eq!(heroic.full_name, HEROIC_ID);
    assert_eq!(heroic.name, "HeroicGamesLauncher");
    assert_eq!(heroic.owner.login, "Heroic-Games-Launcher");
    assert_eq!(heroic.stargazers_count, 12171);
    assert_eq!(
        heroic.license.as_ref().unwrap().spdx_id.as_deref(),
        Some("GPL-3.0")
    );
    assert!(heroic.topics.contains(&"electron".to_string()));
    assert_eq!(heroic.pushed_at.as_deref(), Some("2026-09-09T16:19:01Z"));
    assert!(repos[3].license.is_none() || repos[3].license.as_ref().unwrap().spdx_id.is_some());
}

#[test]
fn heroic_prefers_the_distributions_own_package_kind() {
    let release = github::parse_release(HEROIC).unwrap();
    assert_eq!(release.tag_name, "v2.22.1");
    let pick = |native| github::pick_asset(&release.assets, Some(Arch::X86_64), native).unwrap();
    assert_eq!(
        pick(Some(AssetKind::Pacman)).asset.name,
        "Heroic-2.22.1-linux-x64.pacman"
    );
    assert_eq!(
        pick(Some(AssetKind::Deb)).asset.name,
        "Heroic-2.22.1-linux-amd64.deb"
    );
    assert_eq!(
        pick(Some(AssetKind::Rpm)).asset.name,
        "Heroic-2.22.1-linux-x86_64.rpm"
    );
    let none = pick(None);
    assert_eq!(
        none.asset.name, "Heroic-2.22.1-linux-x86_64.AppImage",
        "no native kind: the AppImage"
    );
    assert_eq!(none.kind, AssetKind::AppImage);
    assert_eq!(none.arch, Some(Arch::X86_64));
    assert!(
        github::pick_asset(&release.assets, Some(Arch::Aarch64), None).is_none(),
        "Heroic publishes nothing for Linux on ARM; the macOS arm64 zip is not it"
    );
}

#[test]
fn neovim_prefers_the_machines_architecture() {
    let release = github::parse_release(NEOVIM).unwrap();
    let x =
        github::pick_asset(&release.assets, Some(Arch::X86_64), Some(AssetKind::Pacman)).unwrap();
    assert_eq!(x.asset.name, "nvim-linux-x86_64.appimage");
    assert_eq!(x.kind, AssetKind::AppImage);
    let a = github::pick_asset(&release.assets, Some(Arch::Aarch64), None).unwrap();
    assert_eq!(a.asset.name, "nvim-linux-arm64.appimage");
    assert_eq!(a.arch, Some(Arch::Aarch64));
    // An archive is found when nothing better exists.
    let archives: Vec<_> = release
        .assets
        .iter()
        .filter(|a| a.name.ends_with(".tar.gz"))
        .cloned()
        .collect();
    let t = github::pick_asset(&archives, Some(Arch::X86_64), None).unwrap();
    assert_eq!(t.asset.name, "nvim-linux-x86_64.tar.gz");
    assert_eq!(t.kind, AssetKind::Archive);
}

#[test]
fn a_repository_maps_to_a_package() {
    let repo = github::parse_search(SEARCH).unwrap().remove(1);
    let release = github::parse_release(HEROIC).unwrap();
    let chosen =
        github::pick_asset(&release.assets, Some(Arch::X86_64), Some(AssetKind::Pacman)).unwrap();
    let p = github::package(&repo, &release, &chosen, &Catalogue::default());
    assert_eq!(p.source, SourceKind::Github);
    assert_eq!(p.id, HEROIC_ID);
    assert_eq!(p.name, "HeroicGamesLauncher");
    assert_eq!(p.kind, PackageKind::App);
    assert_eq!(p.version.as_deref(), Some("2.22.1"));
    assert!(!p.installed);
    assert_eq!(p.developer.as_deref(), Some("Heroic-Games-Launcher"));
    assert_eq!(
        p.homepage.as_deref(),
        Some("https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher")
    );
    assert_eq!(p.licence.as_deref(), Some("GPL-3.0"));
    assert_eq!(p.updated, github::parse_time("2026-09-09T16:19:01Z"));
    assert_eq!(p.download_size, Some(chosen.asset.size));
    assert!((p.popularity.unwrap() - 12171.0 / 20000.0).abs() < 1e-9);
    assert_eq!(p.popularity_label.as_deref(), Some("12\u{2009}171 stars"));
    assert!(
        matches!(&p.icon, Some(Picture::Url(u)) if u.contains("avatars.githubusercontent.com"))
    );
    assert!(
        p.summary
            .as_deref()
            .unwrap()
            .starts_with("A games launcher")
    );
    assert_eq!(
        p.appstream_id, None,
        "a name match never joins editions as certain"
    );
    assert_eq!(fact(&p.facts, "Stars"), Some("12\u{2009}171"));
    assert_eq!(fact(&p.facts, "Latest release"), Some("v2.22.1"));
    assert_eq!(
        fact(&p.facts, "Asset"),
        Some("Heroic-2.22.1-linux-x64.pacman")
    );
    assert_eq!(fact(&p.facts, "Published"), Some("2026-08-09"));
    assert!(
        fact(&p.facts, "Topics")
            .unwrap()
            .contains("epic-games-launcher")
    );
    assert_eq!(fact(&p.facts, "Install"), None);

    let appimage = github::pick_asset(&release.assets, Some(Arch::X86_64), None).unwrap();
    let p = github::package(&repo, &release, &appimage, &Catalogue::default());
    assert!(fact(&p.facts, "Install").unwrap().contains("~/.local/bin"));

    let stars_capped = github::parse_search(SEARCH).unwrap().remove(0);
    let p = github::package(&stars_capped, &release, &appimage, &Catalogue::default());
    assert_eq!(
        p.popularity,
        Some(1.0),
        "23 790 stars is the top of the scale"
    );
}

#[test]
fn a_rate_limit_is_one_sentence() {
    assert!(github::rate_limited(403, Some("0")));
    assert!(github::rate_limited(429, None));
    assert!(!github::rate_limited(403, Some("7")));
    assert!(!github::rate_limited(403, None));
    assert!(!github::rate_limited(200, Some("0")));
    assert_eq!(
        github::RATE_LIMITED,
        "GitHub is rate-limiting searches; try again in a minute."
    );
}

#[test]
fn plans_per_asset_kind() {
    let release = github::parse_release(HEROIC).unwrap();
    let downloads = Path::new("/home/me/.cache/brokey/downloads");
    let home = Path::new("/home/me");
    let steps_for = |name: &str, native: Option<AssetKind>, has_curl: bool| {
        let asset = release.assets.iter().find(|a| a.name == name).unwrap();
        let chosen =
            github::pick_asset(std::slice::from_ref(asset), Some(Arch::X86_64), native).unwrap();
        github::install_steps(&InstallTarget {
            name: "HeroicGamesLauncher",
            chosen: &chosen,
            downloads,
            home,
            native,
            has_curl,
        })
    };

    let pacman = steps_for(
        "Heroic-2.22.1-linux-x64.pacman",
        Some(AssetKind::Pacman),
        true,
    )
    .unwrap();
    assert_eq!(pacman.len(), 2);
    assert_eq!(pacman[0].command.program, "curl");
    assert_eq!(
        pacman[0].title,
        "Downloading Heroic-2.22.1-linux-x64.pacman"
    );
    assert!(!pacman[0].needs_root);
    assert_eq!(pacman[0].weight, 4);
    let path = "/home/me/.cache/brokey/downloads/Heroic-2.22.1-linux-x64.pacman";
    assert_eq!(
        pacman[0].command.args,
        vec![
            "-L",
            "-sS",
            "--fail",
            "--create-dirs",
            "-o",
            path,
            "https://github.com/Heroic-Games-Launcher/HeroicGamesLauncher/releases/download/v2.22.1/Heroic-2.22.1-linux-x64.pacman"
        ]
    );
    assert_eq!(pacman[1].command.program, "pacman");
    assert_eq!(pacman[1].command.args, vec!["-U", "--noconfirm", path]);
    assert!(pacman[1].needs_root);
    assert_eq!(pacman[1].title, "Installing HeroicGamesLauncher");
    assert!(pacman.iter().all(|s| s.source == SourceKind::Github));

    let deb = steps_for("Heroic-2.22.1-linux-amd64.deb", Some(AssetKind::Deb), true).unwrap();
    assert_eq!(deb[1].command.program, "apt-get");
    assert_eq!(deb[1].command.args[..2], ["install", "-y"]);
    assert!(deb[1].command.args[2].ends_with("Heroic-2.22.1-linux-amd64.deb"));
    assert!(deb[1].needs_root);
    assert_eq!(
        deb[1].command.env,
        vec![("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string())]
    );

    let rpm = steps_for("Heroic-2.22.1-linux-x86_64.rpm", Some(AssetKind::Rpm), true).unwrap();
    assert_eq!(rpm[1].command.program, "dnf");
    assert_eq!(rpm[1].command.args[..2], ["install", "-y"]);
    assert!(rpm[1].needs_root);

    let appimage = steps_for(
        "Heroic-2.22.1-linux-x86_64.AppImage",
        Some(AssetKind::Pacman),
        true,
    )
    .unwrap();
    assert_eq!(appimage[1].command.program, "install");
    assert_eq!(
        appimage[1].command.args,
        vec![
            "-Dm755",
            "/home/me/.cache/brokey/downloads/Heroic-2.22.1-linux-x86_64.AppImage",
            "/home/me/.local/bin/HeroicGamesLauncher"
        ]
    );
    assert!(!appimage[1].needs_root);

    let archive = steps_for(
        "Heroic-2.22.1-linux-x64.tar.xz",
        Some(AssetKind::Pacman),
        true,
    )
    .unwrap_err();
    assert!(
        archive
            .message
            .starts_with("This release is an archive, not a package; download it from https://"),
        "{}",
        archive.message
    );
    assert_eq!(archive.source_kind, Some(SourceKind::Github));

    let wrong_distro = steps_for(
        "Heroic-2.22.1-linux-amd64.deb",
        Some(AssetKind::Pacman),
        true,
    )
    .unwrap_err();
    assert!(
        wrong_distro.message.contains("a Debian package"),
        "{}",
        wrong_distro.message
    );

    let no_curl = steps_for(
        "Heroic-2.22.1-linux-x64.pacman",
        Some(AssetKind::Pacman),
        false,
    )
    .unwrap_err();
    assert_eq!(
        no_curl.message,
        "curl is needed to download GitHub releases; install it and try again."
    );

    // A Flatpak bundle: hand-made, Heroic does not publish one.
    let flatpak = github::Asset {
        name: "heroic.flatpak".to_string(),
        size: 5,
        browser_download_url: "https://example.invalid/heroic.flatpak".to_string(),
    };
    let chosen =
        github::pick_asset(&[flatpak], Some(Arch::X86_64), Some(AssetKind::Pacman)).unwrap();
    let steps = github::install_steps(&InstallTarget {
        name: "heroic",
        chosen: &chosen,
        downloads,
        home,
        native: Some(AssetKind::Pacman),
        has_curl: true,
    })
    .unwrap();
    assert_eq!(steps[1].command.program, "flatpak");
    assert_eq!(
        steps[1].command.args[..4],
        ["install", "-y", "--noninteractive", "--user"]
    );
    assert!(!steps[1].needs_root);
}

#[test]
fn the_record_file_drives_installed_and_removal() {
    let dir = tempfile::tempdir().unwrap();
    let source = source(&system("ID=cachyos\nID_LIKE=arch\n"), dir.path());
    assert!(source.installed().unwrap().is_empty());
    assert!(source.records().is_empty());

    source
        .record_install(heroic_record(AssetKind::AppImage))
        .unwrap();
    assert!(dir.path().join("data/github-installs.json").is_file());
    let installed = source.installed().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(installed[0].id, HEROIC_ID);
    assert!(installed[0].installed);
    assert_eq!(installed[0].installed_version.as_deref(), Some("2.22.1"));
    assert_eq!(installed[0].kind, PackageKind::App);
    assert_eq!(
        fact(&installed[0].facts, "Installed as"),
        Some("an AppImage")
    );

    // Recording the same repository again replaces, never duplicates.
    source
        .record_install(heroic_record(AssetKind::AppImage))
        .unwrap();
    assert_eq!(source.records().len(), 1);

    let reference = PackageRef {
        source: SourceKind::Github,
        id: HEROIC_ID.to_string(),
    };
    let remove = source
        .plan(&Op::Remove {
            package: reference.clone(),
        })
        .unwrap();
    assert_eq!(remove.len(), 1);
    assert_eq!(remove[0].command.program, "rm");
    assert!(remove[0].command.args[1].ends_with("/.local/bin/HeroicGamesLauncher"));
    assert!(!remove[0].needs_root);

    source
        .record_install(heroic_record(AssetKind::Pacman))
        .unwrap();
    let e = source.plan(&Op::Remove { package: reference }).unwrap_err();
    assert!(e.message.contains("an Arch package"), "{}", e.message);

    source.forget_install(HEROIC_ID).unwrap();
    assert!(source.installed().unwrap().is_empty());
    let unknown = PackageRef {
        source: SourceKind::Github,
        id: "someone/else".to_string(),
    };
    assert!(source.plan(&Op::Remove { package: unknown }).is_err());
}

#[test]
fn an_install_plan_comes_from_a_remembered_release_and_never_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let reference = PackageRef {
        source: SourceKind::Github,
        id: HEROIC_ID.to_string(),
    };
    let arch = source(&system("ID=cachyos\nID_LIKE=arch\n"), dir.path());
    let e = arch
        .plan(&Op::Install {
            package: reference.clone(),
        })
        .unwrap_err();
    assert!(e.message.contains("not known yet"), "{}", e.message);

    let release = github::parse_release(HEROIC).unwrap();
    arch.remember_release(HEROIC_ID, release.clone());
    let steps = arch
        .plan(&Op::Install {
            package: reference.clone(),
        })
        .unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].command.program, "curl");
    assert_eq!(steps[1].command.program, "pacman");
    assert!(
        steps[0].command.args[5].starts_with(dir.path().join("cache/downloads").to_str().unwrap())
    );
    let update = arch
        .plan(&Op::Update {
            package: reference.clone(),
        })
        .unwrap();
    assert_eq!(update, steps, "an update is the same download and install");

    let debian = source(&system("ID=ubuntu\nID_LIKE=debian\n"), dir.path());
    debian.remember_release(HEROIC_ID, release.clone());
    let steps = debian
        .plan(&Op::Install {
            package: reference.clone(),
        })
        .unwrap();
    assert_eq!(steps[1].command.program, "apt-get");

    let fedora = source(&system("ID=fedora\n"), dir.path());
    fedora.remember_release(HEROIC_ID, release.clone());
    let steps = fedora
        .plan(&Op::Install {
            package: reference.clone(),
        })
        .unwrap();
    assert_eq!(steps[1].command.program, "dnf");

    // Update all: a recorded 2.0.0 with 2.22.1 known is one download and install.
    arch.record_install(InstallRecord {
        version: "2.0.0".to_string(),
        ..heroic_record(AssetKind::Pacman)
    })
    .unwrap();
    let all = arch
        .plan(&Op::UpdateAll {
            source: SourceKind::Github,
        })
        .unwrap();
    assert_eq!(all.len(), 2);
    arch.record_install(heroic_record(AssetKind::Pacman))
        .unwrap();
    assert!(
        arch.plan(&Op::UpdateAll {
            source: SourceKind::Github
        })
        .unwrap()
        .is_empty()
    );
    assert!(
        arch.plan(&Op::Refresh {
            source: SourceKind::Github
        })
        .unwrap()
        .is_empty()
    );
}

#[test]
fn the_source_is_always_available_and_checks_ids() {
    let dir = tempfile::tempdir().unwrap();
    let source = source(&system(""), dir.path());
    let status = source.status();
    assert!(status.available);
    assert_eq!(
        status.detail.as_deref(),
        Some("public API, 10 searches a minute without a token")
    );
    assert!(
        source.search(&Query::new("   ")).unwrap().is_empty(),
        "an empty search asks nothing"
    );
    let e = source.details("not-a-repo").unwrap_err();
    assert!(e.message.contains("owner/repo"), "{}", e.message);
}

/// Needs the network and a share of the unauthenticated limit: one search
/// plus up to eight release look-ups.
#[test]
#[ignore]
fn live_github_search_finds_heroic() {
    let dir = tempfile::tempdir().unwrap();
    let source = source(&brokey_core::system::detect(), dir.path());
    let found = source.search(&Query::new("heroic")).unwrap();
    let heroic = found.iter().find(|p| p.id == HEROIC_ID).unwrap_or_else(|| {
        panic!(
            "Heroic not in {:?}",
            found.iter().map(|p| &p.id).collect::<Vec<_>>()
        )
    });
    assert!(fact(&heroic.facts, "Asset").unwrap().contains("linux"));
    assert!(heroic.version.is_some());

    let plan = source
        .plan(&Op::Install {
            package: heroic.reference(),
        })
        .unwrap();
    assert_eq!(
        plan.len(),
        2,
        "the search made the release known; no second fetch"
    );

    let again = source.search(&Query::new("heroic")).unwrap();
    assert_eq!(
        again.len(),
        found.len(),
        "the second search comes from the cache"
    );

    let details = source.details(HEROIC_ID).unwrap();
    assert!(
        details
            .description
            .as_deref()
            .unwrap_or("")
            .starts_with("<p>")
    );
    eprintln!(
        "{} results with a Linux asset: {}",
        found.len(),
        found
            .iter()
            .map(|p| format!("{} ({})", p.id, fact(&p.facts, "Asset").unwrap_or("?")))
            .collect::<Vec<_>>()
            .join(", ")
    );
}
