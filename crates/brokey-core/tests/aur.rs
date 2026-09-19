//! The AUR source against fixtures captured from the real RPC
//! (`tests/fixtures/aur/`), plus `live_*` tests that ask aur.archlinux.org
//! and read this machine's pacman database.

#![cfg(unix)]

use brokey_core::appstream::{Catalogue, Component};
use brokey_core::http::Client;
use brokey_core::sources::linux::alpmdb::Desc;
use brokey_core::sources::linux::aur::{self, Aur, Helper, Paths, RpcPackage};
use brokey_core::system::from_os_release;
use brokey_core::{Op, PackageKind, PackageRef, Picture, Query, Source, SourceKind, SystemInfo};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/aur");

fn fixture(name: &str) -> String {
    std::fs::read_to_string(Path::new(FIXTURES).join(name))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn steam() -> Vec<RpcPackage> {
    aur::parse_response(&fixture("search-steam.json")).unwrap()
}

fn paru() -> RpcPackage {
    aur::parse_response(&fixture("info-paru.json"))
        .unwrap()
        .remove(0)
}

fn arch() -> SystemInfo {
    from_os_release("ID=cachyos\nID_LIKE=arch\nPRETTY_NAME=\"CachyOS\"\n")
}

fn debian() -> SystemInfo {
    from_os_release("ID=debian\nPRETTY_NAME=\"Debian\"\n")
}

/// An AUR source with no network worth speaking of and no databases.
fn offline(system: &SystemInfo, helper: Helper, tmp: &Path) -> Aur {
    offline_with(system, helper, tmp, Catalogue::default())
}

fn offline_with(system: &SystemInfo, helper: Helper, tmp: &Path, catalogue: Catalogue) -> Aur {
    let client = Arc::new(Client::new(tmp.join("http")));
    Aur::new(system, client, Arc::new(catalogue))
        .with_helper(helper)
        .with_paths(Paths {
            pacman_conf: tmp.join("pacman.conf"),
            local_db: tmp.join("local"),
            sync_dir: tmp.join("sync"),
            cache: tmp.join("cache/aur"),
        })
}

/// A search envelope the way aurweb writes one, for the scripted RPC.
fn envelope(results: &[(&str, &str)]) -> String {
    let results: Vec<serde_json::Value> = results
        .iter()
        .map(|(name, description)| {
            serde_json::json!({
                "Name": name, "Description": description, "Version": "1.0-1",
                "PackageBase": name, "URL": null, "NumVotes": 3, "Popularity": 0.5,
                "OutOfDate": null, "Maintainer": "someone", "FirstSubmitted": 1,
                "LastModified": 2, "URLPath": format!("/cgit/aur.git/snapshot/{name}.tar.gz"), "ID": 7
            })
        })
        .collect();
    serde_json::json!({"version": 5, "type": "search", "resultcount": results.len(), "results": results})
        .to_string()
}

/// aurweb's refusal of a term with more than 5000 hits, verbatim.
fn too_many() -> String {
    r#"{"version":5,"type":"error","resultcount":0,"results":[],"error":"Too many package results."}"#
        .to_string()
}

/// The word an RPC search URL asks for.
fn searched_word(url: &str) -> Option<&str> {
    url.split("/search/").nth(1)?.split('?').next()
}

fn desc(name: &str, version: &str, extra: &str) -> Desc {
    Desc::parse(&format!(
        "%NAME%\n{name}\n\n%VERSION%\n{version}\n\n%DESC%\nA {name}\n\n{extra}"
    ))
}

/// A tiny pacman layout: a local database with the given packages and one
/// `extra` sync database with the given names, in `tmp`.
fn fake_dbs(tmp: &Path, local: &[(&str, &str, &str)], sync: &[&str]) {
    std::fs::write(
        tmp.join("pacman.conf"),
        "[options]\n[extra]\nInclude = /dev/null\n",
    )
    .unwrap();
    for (name, version, extra) in local {
        let dir = tmp.join("local").join(format!("{name}-{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("desc"),
            format!("%NAME%\n{name}\n\n%VERSION%\n{version}\n\n%DESC%\nA {name}\n\n{extra}"),
        )
        .unwrap();
    }
    let mut builder = tar::Builder::new(Vec::new());
    for name in sync {
        let text = format!("%NAME%\n{name}\n\n%VERSION%\n1.0-1\n\n%DESC%\nfrom extra\n");
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("{name}-1.0-1/desc"), text.as_bytes())
            .unwrap();
    }
    std::fs::create_dir_all(tmp.join("sync")).unwrap();
    std::fs::write(tmp.join("sync/extra.db"), builder.into_inner().unwrap()).unwrap();
}

fn aur_ref(name: &str) -> PackageRef {
    PackageRef {
        source: SourceKind::Aur,
        id: name.to_string(),
    }
}

// ---- scoring ---------------------------------------------------------

#[test]
fn text_score_ranks_exact_prefix_contains_then_description() {
    assert_eq!(aur::score("steam", "steam", None, 0.0), 1.0);
    assert_eq!(aur::score("steam", "steam-appimage", None, 0.0), 1.0);
    assert_eq!(aur::score("steam", "steam-native-runtime", None, 0.0), 0.9);
    assert_eq!(aur::score("steam", "python-steam", None, 0.0), 0.7);
    assert_eq!(
        aur::score("steam", "protonup-qt", Some("Manage Proton for Steam"), 0.0),
        0.4
    );
    assert_eq!(
        aur::score("steam", "unrelated", Some("nothing here"), 0.0),
        0.0
    );
    assert_eq!(
        aur::score("Steam", "STEAMCMD", None, 0.0),
        0.9,
        "case does not matter"
    );
}

#[test]
fn popularity_boost_is_logarithmic_and_capped() {
    let none = aur::score("x", "x", None, 0.0);
    let some = aur::score("x", "x", None, 4.0);
    let capped = aur::score("x", "x", None, 9.0);
    let huge = aur::score("x", "x", None, 1.0e9);
    assert_eq!(none, 1.0);
    assert!(
        (some - (1.0 + 5.0f64.log10() / 3.0)).abs() < 1e-9,
        "log10(5)/3 = {}",
        some - 1.0
    );
    assert!(
        (capped - 1.3).abs() < 1e-9,
        "log10(10)/3 is over the cap, got {}",
        capped - 1.0
    );
    assert!(
        (huge - 1.3).abs() < 1e-9,
        "capped at 0.3, got {}",
        huge - 1.0
    );
    assert_eq!(
        aur::score("x", "x", None, -5.0),
        1.0,
        "a negative figure boosts nothing"
    );
}

#[test]
fn edition_suffixes_are_stripped_for_the_exact_match() {
    assert_eq!(aur::base_name("yay-bin"), "yay");
    assert_eq!(aur::base_name("yay-git"), "yay");
    assert_eq!(aur::base_name("steam-appimage"), "steam");
    assert_eq!(aur::base_name("foo-git-bin"), "foo");
    assert_eq!(
        aur::base_name("steam-native-runtime"),
        "steam-native-runtime"
    );
    assert_eq!(
        aur::base_name("-bin"),
        "-bin",
        "a name that is only a suffix stays"
    );
    assert_eq!(aur::score("yay", "yay-bin", None, 0.0), 1.0);
    assert_eq!(aur::score("steam", "steam-native-runtime", None, 0.0), 0.9);
}

#[test]
fn ranking_the_steam_fixture_orders_by_score_and_respects_the_limit() {
    let found = steam();
    assert!(found.len() >= 10, "the fixture holds about ten results");
    let ranked = aur::rank("steam", found.clone(), 200);
    assert_eq!(ranked.len(), found.len(), "every result mentions steam");
    let scores: Vec<f64> = ranked
        .iter()
        .map(|p| aur::score("steam", &p.name, p.description.as_deref(), p.popularity))
        .collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "not sorted: {scores:?}"
    );
    let names: Vec<&str> = ranked.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"steam-appimage"));
    assert!(names.contains(&"steamcmd"));
    // Text tier first among equals in popularity: the exact base match
    // scores above every prefix match with no more popularity than it.
    let appimage = ranked.iter().find(|p| p.name == "steam-appimage").unwrap();
    let native = ranked
        .iter()
        .find(|p| p.name == "steam-native-runtime")
        .unwrap();
    let s = |p: &RpcPackage| aur::score("steam", &p.name, p.description.as_deref(), 0.0);
    assert!(s(appimage) > s(native));
    assert_eq!(aur::rank("steam", found, 3).len(), 3);
}

#[test]
fn several_words_must_all_match_and_join_with_hyphens() {
    let found = steam();
    let ranked = aur::rank("steam native", found.clone(), 200);
    let names: Vec<&str> = ranked.iter().map(|p| p.name.as_str()).collect();
    assert!(names.contains(&"steam-native-runtime"), "{names:?}");
    assert!(
        !names.contains(&"steamcmd"),
        "steamcmd says nothing about native: {names:?}"
    );
    assert_eq!(
        aur::score("steam native runtime", "steam-native-runtime", None, 0.0),
        1.0
    );
    assert!(aur::rank("steam zzzzqqq", found, 200).is_empty());
}

// ---- mapping -----------------------------------------------------------

#[test]
fn paru_info_maps_to_a_package() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    let rpc = paru();
    let p = source.to_package(&rpc);
    assert_eq!(p.source, SourceKind::Aur);
    assert_eq!(p.id, "paru");
    assert_eq!(p.name, "paru");
    assert_eq!(
        p.kind,
        PackageKind::Package,
        "no catalogue entry, so a package"
    );
    assert_eq!(p.summary.as_deref(), Some("Feature packed AUR helper"));
    assert_eq!(p.version.as_deref(), Some(rpc.version.as_str()));
    assert_eq!(p.repo.as_deref(), Some("aur"));
    assert_eq!(
        p.homepage.as_deref(),
        Some("https://github.com/morganamilo/paru")
    );
    assert_eq!(p.developer.as_deref(), Some("Morganamilo"));
    assert_eq!(p.updated, Some(rpc.last_modified));
    assert_eq!(p.licence.as_deref(), Some("GPL-3.0-or-later"));
    assert!(!p.out_of_date);
    assert!(!p.installed);
    let pop = p.popularity.unwrap();
    assert!((pop - (rpc.popularity / 50.0).min(1.0)).abs() < 1e-9);
    assert_eq!(
        p.popularity_label.as_deref(),
        Some(format!("{} votes", rpc.num_votes).as_str())
    );

    let fact = |k: &str| {
        p.facts
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(fact("Maintainer"), Some("Morganamilo"));
    assert_eq!(fact("Votes"), Some(rpc.num_votes.to_string().as_str()));
    assert_eq!(
        fact("Popularity"),
        Some(format!("{:.2}", rpc.popularity).as_str())
    );
    assert_eq!(fact("Package base"), Some("paru"));
    assert_eq!(fact("First submitted"), Some("2020-10-19"));
    assert_eq!(
        fact("Last modified"),
        Some(aur::date(rpc.last_modified).as_str())
    );
    assert_eq!(fact("Out of date since"), None);
    assert_eq!(fact("Licence"), Some("GPL-3.0-or-later"));
    assert_eq!(fact("Depends"), Some(rpc.depends.join(", ").as_str()));
    assert_eq!(
        fact("Make depends"),
        Some(rpc.make_depends.join(", ").as_str())
    );
    assert_eq!(
        fact("Opt depends"),
        Some(rpc.opt_depends.join(", ").as_str())
    );
    assert_eq!(
        fact("AUR page"),
        Some("https://aur.archlinux.org/packages/paru")
    );
    let keys: Vec<&str> = p.facts.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys[0], "Maintainer");
    assert_eq!(keys.last().copied(), Some("AUR page"));
    for (k, v) in &p.facts {
        assert!(
            !k.contains('\u{2014}') && !v.contains('\u{2014}'),
            "no em dashes in {k}"
        );
    }
}

#[test]
fn orphaned_and_out_of_date_packages_say_so() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    let found = steam();
    let orphan = found
        .iter()
        .find(|p| p.maintainer.is_none() && p.out_of_date.is_some())
        .expect("the fixture keeps one orphaned, flagged package");
    let p = source.to_package(orphan);
    assert_eq!(p.developer.as_deref(), Some("orphan"));
    assert!(p.out_of_date);
    let fact = |k: &str| {
        p.facts
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(fact("Maintainer"), Some("none (orphaned)"));
    assert_eq!(
        fact("Out of date since"),
        Some(aur::date(orphan.out_of_date.unwrap()).as_str())
    );
    assert_eq!(fact("Licence"), None, "a search record carries no licence");
    assert_eq!(fact("Depends"), None);
}

#[test]
fn a_search_record_without_a_licence_leaves_the_field_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    let p = source.to_package(&steam()[0]);
    assert_eq!(p.licence, None);
    assert!(p.facts.iter().all(|(k, _)| k != "Licence"));
}

#[test]
fn installed_merges_the_aur_record_over_the_local_one() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    let local = vec![
        desc(
            "mystery",
            "1.0-1",
            "%URL%\nhttps://example.org\n\n%SIZE%\n4096\n\n%LICENSE%\nMIT\n",
        ),
        desc("paru", "2.0.4-1", "%SIZE%\n8192\n"),
    ];
    let info = vec![paru()];
    let listed = source.installed_from(&local, Some(&info));
    assert_eq!(listed.len(), 2);

    let p = listed.iter().find(|p| p.name == "paru").unwrap();
    assert!(p.installed);
    assert_eq!(p.installed_version.as_deref(), Some("2.0.4-1"));
    assert_eq!(
        p.version.as_deref(),
        Some(paru().version.as_str()),
        "the AUR's version"
    );
    assert_eq!(p.summary.as_deref(), Some("Feature packed AUR helper"));
    assert_eq!(p.repo.as_deref(), Some("aur"));
    assert_eq!(p.installed_size, Some(8192));
    assert!(p.popularity_label.as_deref().unwrap().ends_with(" votes"));

    let m = listed.iter().find(|p| p.name == "mystery").unwrap();
    assert!(m.installed);
    assert_eq!(m.installed_version.as_deref(), Some("1.0-1"));
    assert_eq!(m.version.as_deref(), Some("1.0-1"));
    assert_eq!(m.repo.as_deref(), Some("local"));
    assert_eq!(m.summary.as_deref(), Some("A mystery"));
    assert_eq!(m.homepage.as_deref(), Some("https://example.org"));
    assert_eq!(m.licence.as_deref(), Some("MIT"));
    assert_eq!(m.installed_size, Some(4096));
    assert!(
        m.facts.iter().any(|(_, v)| v.starts_with("Not in the AUR")),
        "{:?}",
        m.facts
    );

    // With no answer from the AUR nothing is called local: it is not known.
    let unknown = source.installed_from(&local, None);
    assert!(unknown.iter().all(|p| p.repo.as_deref() == Some("aur")));
    assert!(unknown.iter().all(|p| p.installed));
}

// ---- updates -----------------------------------------------------------

fn record(name: &str, version: &str) -> RpcPackage {
    RpcPackage {
        name: name.to_string(),
        version: version.to_string(),
        package_base: name.to_string(),
        description: Some(format!("The {name} package")),
        last_modified: 1_700_000_000,
        ..RpcPackage::default()
    }
}

#[test]
fn updates_follow_vercmp_and_vcs_packages_only_when_the_aur_is_newer() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    let info = vec![
        record("bauh", "0.10.7-1"),
        record("onedrive-abraunegg", "2.5.11-1"),
        record("howdy-beta-git", "2.6.1.r200.gabcdef0-1"),
        record("hid-ite8291r3-dkms-git", "r20.1234567-1"),
        record("brokey-bin", "0.2.0-1"),
        record("epoch-thing", "1:1.0-1"),
    ];
    let local = [
        ("bauh", "0.10.6-1"),
        ("onedrive-abraunegg", "2.5.11-1"),
        ("howdy-beta-git", "2.6.1.r273.gd3ab993-1"),
        ("hid-ite8291r3-dkms-git", "r16.961702d-1"),
        ("brokey-bin", "0.1.0-1"),
        ("epoch-thing", "2.0-1"),
        ("not-in-aur", "1.0-1"),
    ];
    let updates = source.updates_from(&local, &info);
    let names: Vec<&str> = updates.iter().map(|u| u.name.as_str()).collect();
    assert!(names.contains(&"bauh"), "{names:?}");
    assert!(
        !names.contains(&"onedrive-abraunegg"),
        "same version is no update"
    );
    assert!(
        !names.contains(&"howdy-beta-git"),
        "a VCS package built past the AUR's pkgver looks current"
    );
    assert!(
        names.contains(&"hid-ite8291r3-dkms-git"),
        "a VCS package whose AUR pkgver moved on is an update"
    );
    assert!(names.contains(&"epoch-thing"), "an epoch wins");
    assert!(!names.contains(&"not-in-aur"));

    let bauh = updates.iter().find(|u| u.name == "bauh").unwrap();
    assert_eq!(bauh.package, aur_ref("bauh"));
    assert_eq!(bauh.from.as_deref(), Some("0.10.6-1"));
    assert_eq!(bauh.to, "0.10.7-1");
    assert_eq!(bauh.published, Some(1_700_000_000));
    assert!(!bauh.is_self);
    assert!(
        updates
            .iter()
            .find(|u| u.name == "brokey-bin")
            .unwrap()
            .is_self
    );
    assert!(aur::is_self("brokey"));
    assert!(!aur::is_self("brokey-git"));
    assert!(aur::is_vcs("foo-git") && aur::is_vcs("foo-svn") && !aur::is_vcs("foo-bin"));
}

// ---- status ------------------------------------------------------------

#[test]
fn status_needs_arch_and_a_builder() {
    let tmp = tempfile::tempdir().unwrap();
    let s = offline(&debian(), Helper::Paru("2.1.0".into()), tmp.path()).status();
    assert!(!s.available);
    assert_eq!(
        s.reason.as_deref(),
        Some("The AUR needs an Arch-based system.")
    );
    assert_eq!(s.kind, SourceKind::Aur);

    let s = offline(&arch(), Helper::None, tmp.path()).status();
    assert!(!s.available);
    assert_eq!(
        s.reason.as_deref(),
        Some(
            "makepkg is not installed, so AUR packages cannot be built. Install base-devel and try again."
        )
    );
    assert_eq!(s.reason.as_deref(), Some(aur::NO_BUILDER));

    let s = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path()).status();
    assert!(s.available);
    assert_eq!(s.reason, None);
    assert_eq!(s.detail.as_deref(), Some("paru 2.1.0"));

    let s = offline(&arch(), Helper::Yay("12.4.2".into()), tmp.path()).status();
    assert_eq!(s.detail.as_deref(), Some("yay 12.4.2"));

    let s = offline(&arch(), Helper::Makepkg, tmp.path()).status();
    assert!(s.available);
    assert_eq!(s.detail.as_deref(), Some("makepkg"));
}

// ---- plans -------------------------------------------------------------

#[test]
fn paru_builds_in_the_user_session_with_pkexec() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    let steps = source
        .plan(&Op::Install {
            package: aur_ref("paru"),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    let s = &steps[0];
    assert_eq!(s.source, SourceKind::Aur);
    assert_eq!(s.title, "Building paru from the AUR");
    assert_eq!(s.command.program, "paru");
    assert_eq!(
        s.command.args,
        [
            "-S",
            "--noconfirm",
            "--needed",
            "--sudo",
            "pkexec",
            "--skipreview",
            "paru"
        ]
    );
    assert_eq!(
        s.command.env,
        [("LC_ALL".to_string(), "C.UTF-8".to_string())]
    );
    assert_eq!(s.command.cwd, None);
    assert!(!s.needs_root);
    assert_eq!(s.weight, 8);
    let update = source
        .plan(&Op::Update {
            package: aur_ref("paru"),
        })
        .unwrap();
    assert_eq!(update, steps, "an update is the same build");
}

#[test]
fn yay_builds_the_same_way_without_skipreview() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Yay("12.4.2".into()), tmp.path());
    let steps = source
        .plan(&Op::Install {
            package: aur_ref("bauh"),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].command.program, "yay");
    assert_eq!(
        steps[0].command.args,
        ["-S", "--noconfirm", "--needed", "--sudo", "pkexec", "bauh"]
    );
    assert!(!steps[0].needs_root);
    assert_eq!(steps[0].title, "Building bauh from the AUR");
}

#[test]
fn the_builtin_path_clones_then_runs_makepkg_as_the_user() {
    let tmp = tempfile::tempdir().unwrap();
    // pacman is installed; git and cargo are in a repository but missing;
    // libalpm.so is a virtual name nothing here carries.
    fake_dbs(
        tmp.path(),
        &[("pacman", "7.0.0-1", "")],
        &["git", "cargo", "pacman"],
    );
    let source = offline(&arch(), Helper::Makepkg, tmp.path());
    source.remember(&[paru()]);
    let steps = source
        .plan(&Op::Install {
            package: aur_ref("paru"),
        })
        .unwrap();
    assert_eq!(steps.len(), 3, "{steps:#?}");

    let deps = &steps[0];
    assert_eq!(deps.command.program, "pacman");
    assert_eq!(
        deps.command.args,
        ["-S", "--needed", "--noconfirm", "--asdeps", "git", "cargo"]
    );
    assert!(deps.needs_root);
    assert_eq!(deps.title, "Installing build dependencies for paru");

    let clone = &steps[1];
    let dir = tmp.path().join("cache/aur/paru");
    assert_eq!(clone.command.program, "git");
    assert_eq!(
        clone.command.args,
        [
            "clone",
            "--depth",
            "1",
            "https://aur.archlinux.org/paru.git",
            &dir.display().to_string()
        ]
    );
    assert!(!clone.needs_root);
    assert_eq!(clone.title, "Fetching paru");

    let build = &steps[2];
    let conf = tmp.path().join("cache/aur/makepkg.conf");
    assert_eq!(build.command.program, "makepkg");
    assert_eq!(
        build.command.args,
        [
            "--config",
            &conf.display().to_string(),
            "-s",
            "-i",
            "--noconfirm",
            "--needed"
        ]
    );
    assert_eq!(build.command.cwd.as_deref(), Some(dir.as_path()));
    assert!(
        !build.needs_root,
        "makepkg refuses root and a PKGBUILD is untrusted shell"
    );
    assert_eq!(build.weight, 8);
    assert_eq!(build.title, "Building paru");
    assert!(
        build
            .command
            .env
            .contains(&("LC_ALL".to_string(), "C.UTF-8".to_string()))
    );
    assert!(
        !build.command.env.iter().any(|(k, _)| k == "PACMAN_AUTH"),
        "makepkg ignores it in the environment"
    );

    let text = std::fs::read_to_string(&conf).unwrap();
    assert!(text.contains("source /etc/makepkg.conf\n"));
    assert!(text.trim_end().ends_with("PACMAN_AUTH=(pkexec)"));

    // A second plan for a base already checked out pulls instead of cloning.
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let again = source
        .plan(&Op::Install {
            package: aur_ref("paru"),
        })
        .unwrap();
    assert_eq!(
        again[1].command.args[..3],
        [
            "-C".to_string(),
            dir.display().to_string(),
            "pull".to_string()
        ]
    );
}

#[test]
fn the_builtin_path_skips_the_dependency_step_when_nothing_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    fake_dbs(
        tmp.path(),
        &[
            ("pacman", "7.0.0-1", ""),
            ("git", "2.50.0-1", ""),
            ("rustup", "1.28-1", "%PROVIDES%\ncargo\nrust\n"),
        ],
        &["git", "cargo", "pacman", "rust"],
    );
    let source = offline(&arch(), Helper::Makepkg, tmp.path());
    source.remember(&[paru()]);
    let steps = source
        .plan(&Op::Install {
            package: aur_ref("paru"),
        })
        .unwrap();
    assert_eq!(
        steps.len(),
        2,
        "rustup provides cargo, so nothing to install: {steps:#?}"
    );
    assert_eq!(steps[0].command.program, "git");
    assert_eq!(steps[1].command.program, "makepkg");
}

#[test]
fn no_builder_means_no_plan_and_a_sentence() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::None, tmp.path());
    let e = source
        .plan(&Op::Install {
            package: aur_ref("paru"),
        })
        .unwrap_err();
    assert_eq!(
        e.message,
        aur::NO_BUILDER,
        "the plan and the status say the same sentence"
    );
    assert!(e.message.ends_with("Install base-devel and try again."));
}

#[test]
fn a_plan_refuses_a_name_shaped_like_an_option_and_another_source_s_package() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    // paru would read -Rs as a second operation and --sudo=... as its own
    // option in the user session: neither is a name.
    for bad in ["-Rs", "--sudo=/tmp/evil", "Paru", "a b", ""] {
        for op in [
            Op::Install {
                package: aur_ref(bad),
            },
            Op::Update {
                package: aur_ref(bad),
            },
            Op::Remove {
                package: aur_ref(bad),
            },
        ] {
            let e = source.plan(&op).unwrap_err();
            assert_eq!(
                e.message,
                format!("{bad} is not a name the AUR accepts."),
                "{op:?}"
            );
            assert_eq!(e.source_kind, Some(SourceKind::Aur));
        }
    }
    let pacman_s = PackageRef {
        source: SourceKind::Pacman,
        id: "paru".to_string(),
    };
    let e = source
        .plan(&Op::Install {
            package: pacman_s.clone(),
        })
        .unwrap_err();
    assert_eq!(e.message, "paru belongs to pacman, not to the AUR.");
    assert!(source.plan(&Op::Remove { package: pacman_s }).is_err());
    // A proper name still plans.
    assert_eq!(
        source
            .plan(&Op::Install {
                package: aur_ref("python-pyqt6.sip"),
            })
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_common_word_is_passed_over_for_the_next_and_the_last_one_says_what_to_do() {
    let tmp = tempfile::tempdir().unwrap();
    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log = asked.clone();
    let source =
        offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path()).with_rpc(move |url: &str| {
            let word = searched_word(url).unwrap_or("").to_string();
            log.lock().unwrap().push(word.clone());
            match word.as_str() {
                "python" | "lib" | "git" => Ok(too_many()),
                "dlib" => Ok(envelope(&[
                    ("python-dlib", "Python bindings for dlib"),
                    ("dlib-git", "A toolkit for machine learning"),
                ])),
                "qqqq" => Err(brokey_core::Error::new("could not reach aur.archlinux.org")),
                other => panic!("{other} was never meant to be asked"),
            }
        });
    // "python" is refused, "dlib" answers, and rank keeps the record that
    // carries both words.
    let found = source.search(&Query::new("python dlib")).unwrap();
    let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["python-dlib"]);
    assert_eq!(asked.lock().unwrap().as_slice(), ["python", "dlib"]);
    assert_eq!(
        found[0].summary.as_deref(),
        Some("Python bindings for dlib")
    );

    // Every word too common: the sentence names the query and what to do.
    asked.lock().unwrap().clear();
    let e = source.search(&Query::new("python lib")).unwrap_err();
    assert_eq!(
        e.message,
        "The AUR has too many packages matching python lib. Add another word to narrow the search."
    );
    assert_eq!(e.source_kind, Some(SourceKind::Aur));
    assert_eq!(asked.lock().unwrap().as_slice(), ["python", "lib"]);

    // One common word alone, the case a search box sees on every keystroke.
    let e = source.search(&Query::new("  Python ")).unwrap_err();
    assert!(e.message.contains("matching Python."), "{}", e.message);

    // A word too short for aurweb is not asked, even after an overflow.
    asked.lock().unwrap().clear();
    assert!(source.search(&Query::new("git a")).is_err());
    assert_eq!(asked.lock().unwrap().as_slice(), ["git"]);

    // Any other failure is reported as it is, not passed over.
    let e = source.search(&Query::new("qqqq")).unwrap_err();
    assert!(
        e.message
            .starts_with("The AUR did not answer: could not reach"),
        "{}",
        e.message
    );
}

#[test]
fn a_search_hit_is_installed_only_when_it_is_foreign() {
    let tmp = tempfile::tempdir().unwrap();
    // paru is installed from a repository (CachyOS ships it); bauh is
    // installed and no repository has it, so bauh is the AUR's.
    fake_dbs(
        tmp.path(),
        &[
            ("paru", "2.1.0-2", "%SIZE%\n7500000\n"),
            ("bauh", "0.10.7-1", "%SIZE%\n1000\n"),
        ],
        &["paru", "pacman"],
    );
    let source =
        offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path()).with_rpc(|url: &str| {
            match searched_word(url) {
                Some("paru") => Ok(envelope(&[("paru", "Feature packed AUR helper")])),
                Some("bauh") => Ok(envelope(&[("bauh", "Graphical store")])),
                other => panic!("{other:?} was never meant to be asked"),
            }
        });
    let paru = source.search(&Query::new("paru")).unwrap().remove(0);
    assert!(
        !paru.installed,
        "a repository package is the pacman source's to report"
    );
    assert_eq!(paru.installed_version, None);
    assert_eq!(paru.installed_size, None);
    let bauh = source.search(&Query::new("bauh")).unwrap().remove(0);
    assert!(bauh.installed);
    assert_eq!(bauh.installed_version.as_deref(), Some("0.10.7-1"));
    assert_eq!(bauh.installed_size, Some(1000));
}

#[test]
fn the_catalogue_lends_its_id_only_on_the_exact_name() {
    let tmp = tempfile::tempdir().unwrap();
    let firefox = Component {
        id: "org.mozilla.firefox".into(),
        pkgname: Some("firefox".into()),
        name: "Firefox".into(),
        summary: Some("Web browser".into()),
        description: Some("<p>Browse the web.</p>".into()),
        categories: vec!["Network".into(), "WebBrowser".into()],
        icon: Some(Picture::Url("https://example.org/firefox.png".into())),
        is_app: true,
        component_type: "desktop-application".into(),
        ..Default::default()
    };
    let source = offline_with(
        &arch(),
        Helper::Paru("2.1.0".into()),
        tmp.path(),
        Catalogue::from_components(vec![firefox]),
    );
    // firefox-bin is Firefox by the suffix rule, which is the name pass's
    // guess: it draws as the application but carries no AppStream id, so
    // the grouper says "matched by name" rather than claiming certainty.
    let bin = source.to_package(&record("firefox-bin", "143.0-1"));
    assert_eq!(bin.kind, PackageKind::App);
    assert!(matches!(bin.icon, Some(Picture::Url(_))));
    assert_eq!(bin.categories, ["Network", "WebBrowser"]);
    assert_eq!(bin.description.as_deref(), Some("<p>Browse the web.</p>"));
    assert_eq!(bin.appstream_id, None);
    let exact = source.to_package(&record("firefox", "143.0-1"));
    assert_eq!(exact.appstream_id.as_deref(), Some("org.mozilla.firefox"));
    assert_eq!(exact.kind, PackageKind::App);
    let unknown = source.to_package(&record("something-else-bin", "1-1"));
    assert_eq!(unknown.kind, PackageKind::Package);
    assert_eq!(unknown.appstream_id, None);
    assert!(unknown.icon.is_none());
}

#[test]
fn remove_goes_through_the_helper_and_refresh_does_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    let steps = source
        .plan(&Op::Remove {
            package: aur_ref("bauh"),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].command.program, "pacman");
    assert_eq!(steps[0].command.args, ["-Rs", "--noconfirm", "bauh"]);
    assert!(steps[0].needs_root);
    assert_eq!(steps[0].title, "Removing bauh");
    assert!(
        source
            .plan(&Op::Refresh {
                source: SourceKind::Aur
            })
            .unwrap()
            .is_empty()
    );
}

#[test]
fn update_all_is_one_helper_run() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    let steps = source
        .plan(&Op::UpdateAll {
            source: SourceKind::Aur,
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].command.program, "paru");
    assert_eq!(
        steps[0].command.args,
        ["-Sua", "--noconfirm", "--sudo", "pkexec", "--skipreview"]
    );
    assert!(!steps[0].needs_root);
    assert_eq!(steps[0].title, "Updating AUR packages");

    let source = offline(&arch(), Helper::Yay("12.4.2".into()), tmp.path());
    let steps = source
        .plan(&Op::UpdateAll {
            source: SourceKind::Aur,
        })
        .unwrap();
    assert_eq!(
        steps[0].command.args,
        ["-Sua", "--noconfirm", "--sudo", "pkexec"]
    );
}

#[test]
fn a_short_query_asks_nothing_and_returns_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let source = offline(&arch(), Helper::Paru("2.1.0".into()), tmp.path());
    assert!(source.search(&Query::new("")).unwrap().is_empty());
    assert!(source.search(&Query::new("s")).unwrap().is_empty());
    assert!(source.search(&Query::new("  ")).unwrap().is_empty());
}

// ---- live --------------------------------------------------------------

fn live() -> Aur {
    let system = brokey_core::system::detect();
    let client = Client::shared();
    Aur::new(&system, client, Arc::new(Catalogue::default()))
}

#[test]
#[ignore = "asks aur.archlinux.org"]
fn live_search_paru_finds_paru() {
    let source = live();
    let found = source.search(&Query::new("paru")).unwrap();
    assert!(!found.is_empty());
    let paru = found
        .iter()
        .find(|p| p.name == "paru")
        .expect("paru is in the AUR");
    assert_eq!(
        found[0].name,
        "paru",
        "the exact match with 1257 votes comes first: {:?}",
        found.iter().map(|p| &p.name).take(5).collect::<Vec<_>>()
    );
    assert_eq!(paru.repo.as_deref(), Some("aur"));
    assert!(paru.version.is_some());
    assert!(paru.popularity_label.as_deref().unwrap().ends_with("votes"));
    let details = source.details("paru").unwrap();
    assert_eq!(details.licence.as_deref(), Some("GPL-3.0-or-later"));
    let pkgbuild = details
        .facts
        .iter()
        .find(|(k, _)| k == "PKGBUILD")
        .map(|(_, v)| v.as_str())
        .expect("the PKGBUILD");
    assert!(pkgbuild.contains("pkgname=paru"), "{pkgbuild}");
    assert!(details.facts.iter().any(|(k, _)| k == "Depends"));
}

#[test]
#[ignore = "reads this machine's pacman database and asks aur.archlinux.org"]
fn live_installed_lists_this_machine_s_foreign_packages() {
    let source = live();
    if !source.status().available {
        eprintln!("skipped: {:?}", source.status().reason);
        return;
    }
    let listed = source.installed().unwrap();
    let mut names: Vec<&str> = listed.iter().map(|p| p.name.as_str()).collect();
    names.sort_unstable();
    // What pacman calls foreign is the reference: the same set, no more.
    let qm = brokey_core::system::run("pacman", &["-Qm"]).unwrap();
    let mut expected: Vec<&str> = qm
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();
    expected.sort_unstable();
    assert_eq!(names, expected);
    assert!(
        listed
            .iter()
            .all(|p| p.installed && p.installed_version.is_some())
    );
    if let Some(bauh) = listed.iter().find(|p| p.name == "bauh") {
        assert_eq!(bauh.repo.as_deref(), Some("aur"));
        assert!(bauh.popularity_label.is_some(), "the AUR knows bauh");
    }
    eprintln!("{} foreign packages: {}", listed.len(), names.join(", "));
}

#[test]
#[ignore = "reads this machine's pacman database and asks aur.archlinux.org"]
fn live_updates_run() {
    let source = live();
    if !source.status().available {
        return;
    }
    let updates = source.updates().unwrap();
    for u in &updates {
        assert!(brokey_core::vercmp::is_newer(
            &u.to,
            u.from.as_deref().unwrap()
        ));
        eprintln!(
            "{}: {} -> {}",
            u.name,
            u.from.as_deref().unwrap_or("?"),
            u.to
        );
    }
    eprintln!("{} AUR updates", updates.len());
}

/// Capture the fixtures with the store's own client. Writes into
/// `BROKEY_FIXTURE_DIR` when that is set, else into a temporary directory, so
/// running every live test never rewrites the checked-in files by accident.
#[test]
#[ignore = "asks aur.archlinux.org and writes fixture files"]
fn live_capture_fixtures() {
    const KEEP: [&str; 11] = [
        "steam-appimage",
        "steam-native-runtime",
        "steamcmd",
        "steamtinkerlaunch",
        "steam-tui",
        "steam-tui-bin",
        "python-steam",
        "protonup-qt",
        "millennium-bin",
        "appinfo-vdf-git",
        "asf",
    ];
    let dir = std::env::var_os("BROKEY_FIXTURE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("brokey-aur-fixtures"));
    std::fs::create_dir_all(&dir).unwrap();
    let client = Client::shared();

    let search = client
        .get_text(&format!("{}/search/steam?by=name-desc", aur::RPC))
        .unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&search).unwrap();
    let results = v["results"].as_array().cloned().unwrap();
    let kept: Vec<serde_json::Value> = KEEP
        .iter()
        .filter_map(|name| results.iter().find(|r| r["Name"] == *name).cloned())
        .collect();
    assert_eq!(
        kept.len(),
        KEEP.len(),
        "every kept name is still in the AUR"
    );
    v["resultcount"] = serde_json::Value::from(kept.len());
    v["results"] = serde_json::Value::Array(kept);
    std::fs::write(
        dir.join("search-steam.json"),
        serde_json::to_string_pretty(&v).unwrap() + "\n",
    )
    .unwrap();

    let info = client
        .get_text(&format!("{}/info?arg[]=paru", aur::RPC))
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&info).unwrap();
    assert_eq!(v["resultcount"], 1);
    std::fs::write(
        dir.join("info-paru.json"),
        serde_json::to_string_pretty(&v).unwrap() + "\n",
    )
    .unwrap();
    eprintln!("wrote fixtures to {}", dir.display());
}
