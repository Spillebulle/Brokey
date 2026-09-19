//! The Flatpak source against recorded `flatpak` output and recorded Flathub
//! answers, on a machine that need not have flatpak at all. The scripted
//! runner answers exactly the commands the source is expected to run, so
//! these tests also pin which commands those are. The `live_*` tests reach
//! flathub.org and run only when asked for.

#![cfg(unix)]

use brokey_core::appstream::{Catalogue, Component};
use brokey_core::http::Client;
use brokey_core::sources::linux::flatpak::{
    ADD_FLATHUB_NOTICE, FlathubApi, Flatpak, Installation, LiveFlathub, NO_REMOTES, NOT_INSTALLED,
    NOT_INSTALLED_NO_SETUP, NOT_INSTALLED_SEARCHABLE, SETUP_NOTICE, SETUP_SENTENCE,
    ScriptedFlathub, ScriptedRunner, parse_hits, parse_list, parse_operation, parse_progress,
    parse_remote_ls, parse_remotes,
};
use brokey_core::transaction::allow::{Allowed, check_step};
use brokey_core::{
    Op, PackageKind, PackageRef, Picture, Query, Source, SourceKind, SourceSetup, Step, SystemInfo,
};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

const REMOTES_SYSTEM: [&str; 3] = ["remotes", "--system", "--columns=name,url,options"];
const REMOTES_USER: [&str; 3] = ["remotes", "--user", "--columns=name,url,options"];
const LIST: [&str; 3] = [
    "list",
    "--app",
    "--columns=application,name,version,branch,origin,installation,size",
];
const REMOTE_LS: [&str; 4] = [
    "remote-ls",
    "--updates",
    "--app",
    "--columns=application,name,version,branch,origin,download-size",
];

const GIMP: &str = "flathub/app/org.gimp.GIMP/x86_64/stable";
const GIMP_BETA: &str = "flathub-beta/app/org.gimp.GIMP/x86_64/beta";
const STEAM: &str = "flathub/app/com.valvesoftware.Steam/x86_64/stable";
const FIREFOX: &str = "flathub/app/org.mozilla.firefox/x86_64/stable";

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/flatpak")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn json(name: &str) -> Value {
    serde_json::from_str(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn component(
    origin: &str,
    id: &str,
    branch: &str,
    name: &str,
    summary: &str,
    keywords: &[&str],
) -> Component {
    Component {
        id: id.to_string(),
        origin: origin.to_string(),
        bundle: Some(format!("app/{id}/x86_64/{branch}")),
        name: name.to_string(),
        summary: Some(summary.to_string()),
        keywords: keywords.iter().map(|k| k.to_string()).collect(),
        icon: Some(Picture::File(
            format!("/var/lib/flatpak/appstream/{origin}/x86_64/active/icons/128x128/{id}.png")
                .into(),
        )),
        is_app: true,
        ..Component::default()
    }
}

/// What the remotes' catalogues would hold: two editions of GIMP, two more
/// applications, a runtime, and a component from a remote that is no longer
/// configured.
fn catalogue() -> Catalogue {
    let mut gimp = component(
        "flathub",
        "org.gimp.GIMP",
        "stable",
        "GNU Image Manipulation Program",
        "High-end image creation and manipulation",
        &["GIMP", "Photoshop"],
    );
    gimp.description = Some("<p>The catalogue's description.</p>".to_string());
    gimp.developer = Some("The GIMP team".to_string());
    gimp.licence = Some("GPL-3.0+ AND LGPL-3.0+".to_string());
    gimp.homepage = Some("https://www.gimp.org/".to_string());
    gimp.categories = vec!["Graphics".to_string(), "2DGraphics".to_string()];
    gimp.latest_release = Some(("3.2.4".to_string(), Some(1_776_384_000)));
    let runtime = Component {
        id: "org.gnome.Platform".to_string(),
        origin: "flathub".to_string(),
        bundle: Some("runtime/org.gnome.Platform/x86_64/50".to_string()),
        name: "GNOME Application Platform version 50".to_string(),
        is_app: false,
        ..Component::default()
    };
    Catalogue::from_components(vec![
        gimp,
        component(
            "flathub-beta",
            "org.gimp.GIMP",
            "beta",
            "GNU Image Manipulation Program",
            "High-end image creation and manipulation",
            &["GIMP"],
        ),
        component(
            "flathub",
            "com.valvesoftware.Steam",
            "stable",
            "Steam",
            "Launcher for the Steam software distribution service",
            &["games"],
        ),
        component(
            "flathub",
            "org.mozilla.firefox",
            "stable",
            "Firefox",
            "Fast, private and safe web browser",
            &[],
        ),
        component(
            "gone",
            "org.example.GimpClone",
            "stable",
            "Gimp Clone",
            "From a remote that was removed",
            &[],
        ),
        runtime,
    ])
}

/// A machine with flatpak, Flathub and Fedora in the system installation,
/// Flathub's beta remote in the user one, three applications installed and
/// two updates waiting.
fn runner() -> ScriptedRunner {
    ScriptedRunner::present()
        .answers(&REMOTES_SYSTEM, &fixture("remotes-system.txt"))
        .answers(&REMOTES_USER, &fixture("remotes-user.txt"))
        .answers(&LIST, &fixture("list.txt"))
        .answers(&REMOTE_LS, &fixture("remote-ls-updates.txt"))
}

fn flathub() -> ScriptedFlathub {
    ScriptedFlathub {
        search: Some(json("flathub-search-gimp.json")),
        appstream: HashMap::from([(
            "org.gimp.GIMP".to_string(),
            json("flathub-appstream-gimp.json"),
        )]),
        summary: HashMap::from([(
            "org.gimp.GIMP".to_string(),
            json("flathub-summary-gimp.json"),
        )]),
    }
}

fn source() -> Flatpak {
    Flatpak::with_parts("x86_64", runner(), flathub(), catalogue())
}

fn package_ref(id: &str) -> PackageRef {
    PackageRef {
        source: SourceKind::Flatpak,
        id: id.to_string(),
    }
}

fn facts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn args(step: &Step) -> Vec<&str> {
    step.command.args.iter().map(String::as_str).collect()
}

/// Section 12 of the style guide, for every sentence a user could read.
fn house_style(sentence: &str) {
    assert!(!sentence.contains('\u{2014}'), "em dash in {sentence:?}");
    assert!(sentence.ends_with('.'), "no full stop in {sentence:?}");
}

#[test]
fn status_on_this_machine_is_honest_about_flatpak() {
    let source = Flatpak::new(&brokey_core::system::detect(), Client::shared());
    let status = source.status();
    assert_eq!(status.kind, SourceKind::Flatpak);
    match brokey_core::system::which("flatpak") {
        None => {
            assert!(!status.available);
            assert!(status.searchable, "Flathub's API still answers");
            assert!(
                status
                    .reason
                    .as_deref()
                    .is_some_and(|r| r.starts_with(NOT_INSTALLED)),
                "{status:?}"
            );
            assert_eq!(status.detail, None);
        }
        // A machine with flatpak: the status is whatever its remotes say;
        // the rule here is only that it is never silent.
        Some(_) => assert!(status.available || status.reason.is_some(), "{status:?}"),
    }
}

fn arch() -> SystemInfo {
    brokey_core::system::from_os_release("ID=cachyos\nID_LIKE=arch\n")
}

fn without_flatpak() -> Flatpak {
    Flatpak::with_parts("x86_64", ScriptedRunner::absent(), flathub(), catalogue())
        .with_system(&arch())
}

#[test]
fn without_flatpak_it_is_searchable_and_can_be_set_up() {
    let source = without_flatpak();
    let status = source.status();
    assert!(!status.available);
    assert!(status.searchable);
    assert_eq!(status.reason.as_deref(), Some(NOT_INSTALLED_SEARCHABLE));
    assert_eq!(
        status.setup,
        Some(SourceSetup {
            label: "Install Flatpak".to_string(),
            sentence: "Installs Flatpak and adds Flathub, then Flatpak applications can be installed and updated here.".to_string(),
        })
    );
    for sentence in [
        NOT_INSTALLED,
        NOT_INSTALLED_SEARCHABLE,
        NOT_INSTALLED_NO_SETUP,
        SETUP_SENTENCE,
        SETUP_NOTICE,
        ADD_FLATHUB_NOTICE,
    ] {
        house_style(sentence);
    }
    // What needs the command still needs it.
    assert_eq!(source.installed().unwrap_err().message, NOT_INSTALLED);
    assert_eq!(source.updates().unwrap_err().message, NOT_INSTALLED);

    // On a distribution the store has no package manager for, it is still
    // searchable and says it cannot be set up.
    let elsewhere = Flatpak::with_parts("x86_64", ScriptedRunner::absent(), flathub(), catalogue());
    let status = elsewhere.status();
    assert!(status.searchable);
    assert_eq!(status.setup, None);
    assert_eq!(status.reason.as_deref(), Some(NOT_INSTALLED_NO_SETUP));
    assert!(elsewhere.setup().is_none());
}

#[test]
fn without_flatpak_search_comes_from_flathub_as_flathub_refs() {
    let source = Flatpak::with_parts(
        "x86_64",
        ScriptedRunner::absent(),
        flathub(),
        Catalogue::from_components(Vec::new()),
    );
    let found = source.search(&Query::new("gimp")).unwrap();
    let ids: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            GIMP,
            "flathub/app/com.github.vikdevelop.photopea_app/x86_64/stable",
            "flathub/app/com.github.unrud.djpdf/x86_64/stable",
        ]
    );
    let gimp = &found[0];
    assert_eq!(gimp.source, SourceKind::Flatpak);
    assert_eq!(gimp.name, "GNU Image Manipulation Program");
    assert_eq!(gimp.kind, PackageKind::App);
    assert_eq!(gimp.repo.as_deref(), Some("flathub"));
    assert_eq!(gimp.appstream_id.as_deref(), Some("org.gimp.GIMP"));
    assert_eq!(
        gimp.summary.as_deref(),
        Some("High-end image creation and manipulation")
    );
    assert_eq!(gimp.developer.as_deref(), Some("The GIMP team"));
    assert!(matches!(&gimp.icon, Some(Picture::Url(u)) if u.ends_with("org.gimp.GIMP.png")));
    assert!(gimp.popularity.is_some_and(|p| p > 0.6));
    assert!(gimp.sandboxed);
    assert!(!gimp.installed, "nothing is installed without flatpak");

    let arm = Flatpak::with_parts(
        "aarch64",
        ScriptedRunner::absent(),
        flathub(),
        Catalogue::from_components(Vec::new()),
    );
    assert_eq!(
        arm.search(&Query::new("gimp")).unwrap()[0].id,
        "flathub/app/org.gimp.GIMP/aarch64/stable"
    );

    let mut query = Query::new("gimp");
    query.limit = 1;
    assert_eq!(source.search(&query).unwrap().len(), 1);
    assert_eq!(source.search(&Query::new("  ")).unwrap(), Vec::new());

    let offline = Flatpak::with_parts(
        "x86_64",
        ScriptedRunner::absent(),
        ScriptedFlathub::offline(),
        catalogue(),
    );
    let e = offline.search(&Query::new("gimp")).unwrap_err();
    assert_eq!(e.source_kind, Some(SourceKind::Flatpak));
    assert!(
        e.message.starts_with("Flathub's search did not answer"),
        "{}",
        e.message
    );
    house_style(&e.message);
}

#[test]
fn without_flatpak_details_come_from_flathub() {
    let source = Flatpak::with_parts(
        "x86_64",
        ScriptedRunner::absent(),
        flathub(),
        Catalogue::from_components(Vec::new()),
    );
    let p = source.details(GIMP).unwrap();
    assert_eq!(p.name, "GNU Image Manipulation Program");
    assert!(!p.installed);
    assert!(p.description.is_some());
    assert!(!p.screenshots.is_empty());
    assert!(p.facts.iter().any(|(k, v)| k == "Remote" && v == "flathub"));

    let offline = Flatpak::with_parts(
        "x86_64",
        ScriptedRunner::absent(),
        ScriptedFlathub::offline(),
        Catalogue::from_components(Vec::new()),
    );
    let e = offline.details(GIMP).unwrap_err();
    house_style(&e.message);
    assert!(e.message.contains("org.gimp.GIMP"), "{}", e.message);
}

#[test]
fn setting_flatpak_up_installs_the_distribution_s_package_then_adds_flathub() {
    let flatpak = |ops: &[Op]| -> Vec<(SourceKind, String)> {
        ops.iter()
            .map(|op| match op {
                Op::Install { package } => (package.source, package.id.clone()),
                other => panic!("a setup installs, it does not {other:?}"),
            })
            .collect()
    };
    for (os_release, source) in [
        ("ID=cachyos\nID_LIKE=arch\n", SourceKind::Pacman),
        ("ID=arch\n", SourceKind::Pacman),
        ("ID=ubuntu\nID_LIKE=debian\n", SourceKind::Apt),
        ("ID=debian\n", SourceKind::Apt),
        ("ID=fedora\n", SourceKind::Dnf),
        (
            "ID=rocky\nID_LIKE=\"rhel centos fedora\"\n",
            SourceKind::Dnf,
        ),
    ] {
        let system = brokey_core::system::from_os_release(os_release);
        let setup = Flatpak::with_parts("x86_64", ScriptedRunner::absent(), flathub(), catalogue())
            .with_system(&system)
            .setup()
            .unwrap_or_else(|| panic!("{os_release} has a setup"));
        assert_eq!(
            flatpak(&setup.ops),
            [(source, "flatpak".to_string())],
            "{os_release}"
        );
        assert_eq!(setup.notice, SETUP_NOTICE);
        assert_eq!(setup.steps.len(), 1);
        let add = &setup.steps[0];
        assert_eq!(add.source, SourceKind::Flatpak);
        assert_eq!(add.command.program, "flatpak");
        assert_eq!(
            args(add),
            [
                "remote-add",
                "--if-not-exists",
                "--system",
                "flathub",
                "https://dl.flathub.org/repo/flathub.flatpakrepo"
            ]
        );
        assert!(
            add.needs_root,
            "the system installation goes through the helper"
        );
        assert_eq!(
            check_step(add, &Allowed::system()),
            Ok(()),
            "the helper allows exactly this step"
        );
    }
    let nowhere = brokey_core::system::from_os_release("ID=gentoo\n");
    assert!(
        Flatpak::with_parts("x86_64", ScriptedRunner::absent(), flathub(), catalogue())
            .with_system(&nowhere)
            .setup()
            .is_none()
    );

    let mut user = without_flatpak();
    user.installation = Installation::User;
    let setup = user.setup().unwrap();
    let add = &setup.steps[0];
    assert!(
        !add.needs_root,
        "the user's own installation needs no helper"
    );
    assert_eq!(
        args(add),
        [
            "remote-add",
            "--if-not-exists",
            "--user",
            "flathub",
            "https://dl.flathub.org/repo/flathub.flatpakrepo"
        ]
    );
}

#[test]
fn with_flatpak_there_is_nothing_to_set_up_unless_there_is_no_remote() {
    let s = source().with_system(&arch());
    assert!(s.status().available);
    assert_eq!(s.status().setup, None);
    assert!(
        !s.status().searchable,
        "an available source is simply searched"
    );
    assert!(s.setup().is_none());

    let bare = Flatpak::with_parts(
        "x86_64",
        ScriptedRunner::present()
            .answers(&REMOTES_SYSTEM, "")
            .answers(&REMOTES_USER, ""),
        flathub(),
        catalogue(),
    );
    let setup = bare.setup().unwrap();
    assert!(setup.ops.is_empty(), "flatpak is there already");
    assert_eq!(setup.notice, ADD_FLATHUB_NOTICE);
    assert_eq!(args(&setup.steps[0])[0], "remote-add");
    assert_eq!(
        bare.status().setup.map(|s| s.label).as_deref(),
        Some("Add Flathub")
    );
}

#[test]
fn without_flatpak_an_install_plans_into_the_preferred_installation() {
    let steps = without_flatpak()
        .plan(&Op::Install {
            package: package_ref(GIMP),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(
        args(&steps[0]),
        [
            "install",
            "-y",
            "--noninteractive",
            "--system",
            "flathub",
            "app/org.gimp.GIMP/x86_64/stable"
        ]
    );
}

#[test]
fn status_with_remotes_names_them_once_each() {
    let status = source().status();
    assert!(status.available);
    assert_eq!(status.reason, None);
    assert_eq!(
        status.detail.as_deref(),
        Some("flathub, fedora, flathub-beta")
    );

    let remotes = source().remotes().unwrap();
    assert_eq!(remotes.len(), 3);
    assert_eq!(remotes[0].name, "flathub");
    assert_eq!(remotes[0].installation, Installation::System);
    assert!(remotes[0].is_flathub());
    assert_eq!(remotes[1].name, "fedora");
    assert!(!remotes[1].is_flathub());
    assert_eq!(remotes[2].name, "flathub-beta");
    assert_eq!(remotes[2].installation, Installation::User);
    assert!(!remotes[2].is_flathub());
}

#[test]
fn status_without_remotes_is_unavailable_and_search_asks_flathub() {
    let runner = ScriptedRunner::present()
        .answers(&REMOTES_SYSTEM, "")
        .answers(&REMOTES_USER, "");
    let source = Flatpak::with_parts("x86_64", runner, flathub(), catalogue());
    let status = source.status();
    assert!(
        !status.available,
        "a source with nothing to search is not available"
    );
    assert_eq!(status.reason.as_deref(), Some(NO_REMOTES));
    assert_eq!(status.detail.as_deref(), Some("no remotes"));
    house_style(NO_REMOTES);
    assert!(status.searchable);
    let found = source.search(&Query::new("gimp")).unwrap();
    assert_eq!(found[0].id, GIMP, "Flathub's own search stands in");
}

#[test]
fn status_when_flatpak_cannot_list_its_remotes() {
    let failure = "flatpak remotes failed: error: Unable to load summary";
    let both = ScriptedRunner::present()
        .fails(&REMOTES_SYSTEM, failure)
        .fails(&REMOTES_USER, failure);
    let status = Flatpak::with_parts("x86_64", both, flathub(), catalogue()).status();
    assert!(!status.available);
    let reason = status.reason.unwrap();
    assert_eq!(
        reason,
        "Flatpak could not list its remotes (flatpak remotes failed: error: Unable to load summary). Check that flatpak works from a terminal."
    );
    house_style(&reason);

    // One installation failing while the other lists is a warning, not a
    // reason to hide what the other has.
    let one = ScriptedRunner::present()
        .answers(&REMOTES_SYSTEM, &fixture("remotes-system.txt"))
        .fails(&REMOTES_USER, failure);
    let status = Flatpak::with_parts("x86_64", one, flathub(), catalogue()).status();
    assert!(status.available);
    assert_eq!(status.detail.as_deref(), Some("flathub, fedora"));

    // One failing and the other empty could mean anything, so it is an error.
    let other = ScriptedRunner::present()
        .fails(&REMOTES_SYSTEM, failure)
        .answers(&REMOTES_USER, "");
    let status = Flatpak::with_parts("x86_64", other, flathub(), catalogue()).status();
    assert!(!status.available);
    assert!(
        status
            .reason
            .unwrap()
            .starts_with("Flatpak could not list its remotes")
    );
}

#[test]
fn remotes_fixtures_parse_for_each_installation() {
    let system = parse_remotes(&fixture("remotes-system.txt"), Installation::System);
    assert_eq!(system.len(), 2);
    assert_eq!(system[0].name, "flathub");
    assert_eq!(system[0].url, "https://dl.flathub.org/repo/");
    assert_eq!(system[0].installation, Installation::System);
    assert_eq!(system[1].name, "fedora");
    assert_eq!(system[1].url, "oci+https://registry.fedoraproject.org");

    let user = parse_remotes(&fixture("remotes-user.txt"), Installation::User);
    assert_eq!(user.len(), 1);
    assert_eq!(user[0].name, "flathub-beta");
    assert_eq!(user[0].installation, Installation::User);

    // Listing both installations at once, flatpak names the installation
    // in the options cell, and that beats what was asked for.
    let both = parse_remotes(&fixture("remotes-both.txt"), Installation::System);
    let installations: Vec<Installation> = both.iter().map(|r| r.installation).collect();
    assert_eq!(
        installations,
        [
            Installation::System,
            Installation::System,
            Installation::User,
            Installation::User
        ]
    );
}

#[test]
fn list_fixture_parses_every_cell() {
    let rows = parse_list(&fixture("list.txt"));
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].application, "org.gimp.GIMP");
    assert_eq!(rows[0].name, "GNU Image Manipulation Program");
    assert_eq!(rows[0].version.as_deref(), Some("3.2.4"));
    assert_eq!(rows[0].branch, "stable");
    assert_eq!(rows[0].origin, "flathub");
    assert_eq!(rows[0].installation, Some(Installation::System));
    assert_eq!(rows[0].size, Some(268_500_000));
    assert_eq!(
        rows[0].size_text.as_deref(),
        Some("268.5 MB"),
        "GLib's no-break space made plain"
    );
    assert_eq!(rows[1].application, "com.valvesoftware.Steam");
    assert_eq!(rows[1].installation, Some(Installation::User));
    assert_eq!(rows[1].size, Some(1_200_000_000));
    assert_eq!(rows[2].version, None, "an empty cell is no version");
    assert_eq!(rows[2].origin, "flathub-beta");
    assert_eq!(rows[2].size, Some(980));
}

#[test]
fn remote_ls_fixture_parses_every_cell() {
    let rows = parse_remote_ls(&fixture("remote-ls-updates.txt"));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].application, "org.gimp.GIMP");
    assert_eq!(rows[0].version.as_deref(), Some("3.2.6"));
    assert_eq!(rows[0].branch, "stable");
    assert_eq!(rows[0].origin, "flathub");
    assert_eq!(rows[0].download_size, Some(99_700_000));
    assert_eq!(rows[1].application, "org.mozilla.firefox");
    assert_eq!(rows[1].name, "Firefox");
    assert_eq!(rows[1].download_size, Some(107_300_000));
}

#[test]
fn installed_maps_the_list_through_the_catalogue() {
    let packages = source().installed().unwrap();
    assert_eq!(packages.len(), 3);

    let gimp = &packages[0];
    assert_eq!(gimp.source, SourceKind::Flatpak);
    assert_eq!(gimp.id, GIMP);
    assert_eq!(gimp.name, "GNU Image Manipulation Program");
    assert_eq!(gimp.kind, PackageKind::App);
    assert!(gimp.installed);
    assert!(gimp.sandboxed);
    assert_eq!(gimp.installed_version.as_deref(), Some("3.2.4"));
    assert_eq!(gimp.repo.as_deref(), Some("flathub"));
    assert_eq!(gimp.appstream_id.as_deref(), Some("org.gimp.GIMP"));
    assert_eq!(
        gimp.summary.as_deref(),
        Some("High-end image creation and manipulation")
    );
    assert_eq!(gimp.installed_size, Some(268_500_000));
    assert!(
        matches!(gimp.icon, Some(Picture::File(_))),
        "the catalogue's icon"
    );
    assert_eq!(
        gimp.facts,
        facts(&[
            ("Installation", "system"),
            ("Branch", "stable"),
            ("Size", "268.5 MB")
        ])
    );

    let steam = &packages[1];
    assert_eq!(steam.id, STEAM);
    assert_eq!(steam.name, "Steam");
    assert_eq!(steam.installed_version.as_deref(), Some("1.0.0.87"));
    assert_eq!(
        steam.facts,
        facts(&[
            ("Installation", "user"),
            ("Branch", "stable"),
            ("Size", "1.2 GB")
        ])
    );

    let unknown = &packages[2];
    assert_eq!(
        unknown.id,
        "flathub-beta/app/org.example.NoVersion/x86_64/stable"
    );
    assert_eq!(
        unknown.name, "NoVersion",
        "no catalogue entry, so the list's own name"
    );
    assert_eq!(unknown.kind, PackageKind::App);
    assert_eq!(unknown.installed_version, None);
    assert_eq!(unknown.icon, None);
    assert_eq!(unknown.repo.as_deref(), Some("flathub-beta"));
    assert_eq!(
        unknown.appstream_id.as_deref(),
        Some("org.example.NoVersion")
    );
    assert_eq!(
        unknown.facts,
        facts(&[
            ("Installation", "user"),
            ("Branch", "stable"),
            ("Size", "980 bytes")
        ])
    );
}

#[test]
fn ids_carry_the_machine_s_architecture() {
    let source = Flatpak::with_parts("aarch64", runner(), flathub(), catalogue());
    let packages = source.installed().unwrap();
    assert_eq!(packages[0].id, "flathub/app/org.gimp.GIMP/aarch64/stable");
}

#[test]
fn installed_when_flatpak_list_fails_is_an_error_with_a_sentence() {
    let runner = ScriptedRunner::present().fails(
        &LIST,
        "flatpak list failed: error: Unable to open repository",
    );
    let e = Flatpak::with_parts("x86_64", runner, flathub(), catalogue())
        .installed()
        .unwrap_err();
    assert_eq!(
        e.message,
        "Flatpak could not list what is installed (flatpak list failed: error: Unable to open repository)."
    );
    house_style(&e.message);
}

#[test]
fn updates_map_remote_ls_with_the_installed_version() {
    let updates = source().updates().unwrap();
    assert_eq!(updates.len(), 2);

    let gimp = &updates[0];
    assert_eq!(gimp.package, package_ref(GIMP));
    assert_eq!(gimp.name, "GNU Image Manipulation Program");
    assert_eq!(gimp.kind, PackageKind::App);
    assert_eq!(
        gimp.summary.as_deref(),
        Some("High-end image creation and manipulation")
    );
    assert!(gimp.icon.is_some());
    assert_eq!(gimp.from.as_deref(), Some("3.2.4"));
    assert_eq!(gimp.to, "3.2.6");
    assert_eq!(gimp.download_size, Some(99_700_000));
    assert!(!gimp.is_self);

    let firefox = &updates[1];
    assert_eq!(firefox.package.id, FIREFOX);
    assert_eq!(firefox.name, "Firefox");
    assert_eq!(firefox.from, None, "not in the installed list");
    assert_eq!(firefox.to, "143.0.2");
    assert_eq!(firefox.download_size, Some(107_300_000));
}

#[test]
fn updates_when_remote_ls_fails_say_what_to_do() {
    let runner = ScriptedRunner::present().fails(
        &REMOTE_LS,
        "flatpak remote-ls failed: error: Unable to load summary",
    );
    let e = Flatpak::with_parts("x86_64", runner, flathub(), catalogue())
        .updates()
        .unwrap_err();
    assert_eq!(
        e.message,
        "Flatpak could not check for updates (flatpak remote-ls failed: error: Unable to load summary). Check the connection and try again."
    );
    house_style(&e.message);
}

#[test]
fn search_scores_the_catalogue_and_enriches_from_flathub() {
    let found = source().search(&Query::new("gimp")).unwrap();
    assert_eq!(
        found.len(),
        2,
        "both editions of GIMP and nothing from a removed remote: {found:#?}"
    );
    assert!(found.iter().all(|p| p.repo.as_deref() != Some("gone")));

    let gimp = &found[0];
    assert_eq!(gimp.id, GIMP);
    assert_eq!(gimp.name, "GNU Image Manipulation Program");
    assert_eq!(gimp.kind, PackageKind::App);
    assert!(gimp.sandboxed);
    assert!(gimp.installed, "the list says so");
    assert_eq!(gimp.installed_version.as_deref(), Some("3.2.4"));
    assert!((gimp.popularity.unwrap() - 0.64374).abs() < 1e-9);
    assert_eq!(
        gimp.popularity_label.as_deref(),
        Some("64 374 installs last month")
    );
    assert_eq!(
        gimp.updated,
        Some(1_788_654_532),
        "Flathub's updated_at beats the catalogue's release"
    );
    assert_eq!(gimp.developer.as_deref(), Some("The GIMP team"));
    assert!(
        gimp.facts
            .contains(&("Verified".to_string(), "Yes".to_string()))
    );
    assert!(
        gimp.facts
            .contains(&("Installation".to_string(), "system".to_string()))
    );

    let beta = &found[1];
    assert_eq!(beta.id, GIMP_BETA);
    assert_eq!(beta.repo.as_deref(), Some("flathub-beta"));
    assert!(
        !beta.installed,
        "the stable branch being installed says nothing about beta"
    );
    assert_eq!(beta.popularity, None, "Flathub's figures are for Flathub");
    assert!(!beta.facts.iter().any(|(k, _)| k == "Verified"));
}

#[test]
fn search_marks_a_user_installation() {
    let found = source().search(&Query::new("steam")).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, STEAM);
    assert!(found[0].installed);
    assert!(
        found[0]
            .facts
            .contains(&("Installation".to_string(), "user".to_string()))
    );
    assert_eq!(
        found[0].popularity, None,
        "not in the Flathub answer for this term"
    );
}

#[test]
fn search_without_the_network_still_succeeds() {
    let source = Flatpak::with_parts("x86_64", runner(), ScriptedFlathub::offline(), catalogue());
    let found = source.search(&Query::new("gimp")).unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].id, GIMP);
    assert_eq!(found[0].popularity, None);
    assert_eq!(found[0].popularity_label, None);
    assert!(found[0].installed);
    assert!(!found[0].facts.iter().any(|(k, _)| k == "Verified"));
}

#[test]
fn search_without_a_catalogue_falls_back_to_flathub_s_own() {
    let source = Flatpak::with_parts(
        "x86_64",
        runner(),
        flathub(),
        Catalogue::from_components(Vec::new()),
    );
    let found = source.search(&Query::new("gimp")).unwrap();
    let ids: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            GIMP,
            "flathub/app/com.github.vikdevelop.photopea_app/x86_64/stable",
            "flathub/app/com.github.unrud.djpdf/x86_64/stable",
        ],
        "the best match first, then Flathub's order by name"
    );
    let gimp = &found[0];
    assert_eq!(gimp.name, "GNU Image Manipulation Program");
    assert_eq!(gimp.kind, PackageKind::App);
    assert_eq!(gimp.repo.as_deref(), Some("flathub"));
    assert_eq!(
        gimp.summary.as_deref(),
        Some("High-end image creation and manipulation")
    );
    assert_eq!(
        gimp.categories,
        ["graphics", "2DGraphics", "RasterGraphics"]
    );
    assert!(matches!(&gimp.icon, Some(Picture::Url(u)) if u.ends_with("org.gimp.GIMP.png")));
    assert!(gimp.installed);
    assert_eq!(
        gimp.popularity_label.as_deref(),
        Some("64 374 installs last month")
    );
    assert_eq!(
        found[1].popularity_label.as_deref(),
        Some("2 465 installs last month")
    );
    assert_eq!(
        found[2].popularity_label.as_deref(),
        Some("953 installs last month")
    );
}

#[test]
fn search_honours_the_limit_and_an_empty_query() {
    let mut query = Query::new("gimp");
    query.limit = 1;
    assert_eq!(source().search(&query).unwrap().len(), 1);
    assert_eq!(source().search(&Query::new("   ")).unwrap(), Vec::new());
}

#[test]
fn details_merge_the_catalogue_flathub_and_the_installed_list() {
    let p = source().details(GIMP).unwrap();
    assert_eq!(p.id, GIMP);
    assert_eq!(p.name, "GNU Image Manipulation Program");
    assert_eq!(p.kind, PackageKind::App);
    assert!(p.installed);
    assert_eq!(p.installed_version.as_deref(), Some("3.2.4"));
    assert_eq!(
        p.version.as_deref(),
        Some("3.2.4"),
        "Flathub's newest release"
    );
    assert_eq!(p.updated, Some(1_776_384_000));
    assert!(
        p.description
            .as_deref()
            .is_some_and(|d| d.starts_with("<p>") && d.contains("GIMP is an acronym")),
        "Flathub's description markup replaces the catalogue's: {:?}",
        p.description
    );
    assert_eq!(p.licence.as_deref(), Some("GPL-3.0+ AND LGPL-3.0+"));
    assert_eq!(p.homepage.as_deref(), Some("https://www.gimp.org/"));
    assert_eq!(p.developer.as_deref(), Some("The GIMP team"));
    assert!(
        matches!(p.icon, Some(Picture::File(_))),
        "a local icon needs no fetch and is kept"
    );
    assert_eq!(p.download_size, Some(99_676_175));
    assert_eq!(
        p.installed_size,
        Some(268_500_000),
        "what flatpak list measured"
    );

    assert_eq!(p.screenshots.len(), 2);
    let first = &p.screenshots[0];
    assert!(matches!(&first.image, Picture::Url(u) if u.ends_with("image-1_orig.png")));
    assert!(
        matches!(&first.thumbnail, Some(Picture::Url(u)) if u.ends_with("image-1_624x351@1.png"))
    );
    assert_eq!((first.width, first.height), (Some(1920), Some(1080)));
    assert!(
        first
            .caption
            .as_deref()
            .is_some_and(|c| c.starts_with("Scene 4"))
    );

    assert_eq!(
        p.facts,
        facts(&[
            ("Remote", "flathub"),
            ("Branch", "stable"),
            ("Installation", "system"),
            ("Runtime", "org.gnome.Platform/x86_64/50"),
            ("Download size", "99.7 MB"),
            ("Installed size", "268.5 MB"),
            ("Licence", "GPL-3.0+ AND LGPL-3.0+"),
            ("Verified", "Yes"),
        ])
    );
}

#[test]
fn details_without_the_network_come_from_the_catalogue() {
    let source = Flatpak::with_parts("x86_64", runner(), ScriptedFlathub::offline(), catalogue());
    let p = source.details(GIMP).unwrap();
    assert_eq!(p.name, "GNU Image Manipulation Program");
    assert_eq!(
        p.description.as_deref(),
        Some("<p>The catalogue's description.</p>")
    );
    assert!(p.screenshots.is_empty());
    assert_eq!(
        p.facts,
        facts(&[
            ("Remote", "flathub"),
            ("Branch", "stable"),
            ("Installation", "system"),
            ("Installed size", "268.5 MB"),
            ("Licence", "GPL-3.0+ AND LGPL-3.0+"),
        ])
    );
}

#[test]
fn details_of_something_not_installed_and_not_on_flathub() {
    let p = source().details(FIREFOX).unwrap();
    assert_eq!(p.name, "Firefox");
    assert!(!p.installed);
    assert_eq!(
        p.facts,
        facts(&[("Remote", "flathub"), ("Branch", "stable")])
    );
}

#[test]
fn details_of_an_unknown_ref_say_what_to_do() {
    let source = Flatpak::with_parts("x86_64", runner(), ScriptedFlathub::offline(), catalogue());
    let e = source
        .details("flathub/app/org.example.Missing/x86_64/stable")
        .unwrap_err();
    assert_eq!(
        e.message,
        "org.example.Missing is not in any Flatpak remote on this machine. Refresh the Flatpak catalogues and try again."
    );
    house_style(&e.message);

    let e = source.details("org.example.Missing").unwrap_err();
    assert_eq!(
        e.message,
        "org.example.Missing is not a Flatpak ref. A ref looks like flathub/app/org.gimp.GIMP/x86_64/stable."
    );
    house_style(&e.message);
}

#[test]
fn plan_install_runs_in_the_session_and_asks_polkit_itself() {
    let steps = source()
        .plan(&Op::Install {
            package: package_ref(FIREFOX),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    let step = &steps[0];
    assert_eq!(step.source, SourceKind::Flatpak);
    assert_eq!(step.title, "Installing Firefox from flathub");
    assert_eq!(step.command.program, "flatpak");
    assert_eq!(
        args(step),
        [
            "install",
            "-y",
            "--noninteractive",
            "--system",
            "flathub",
            "app/org.mozilla.firefox/x86_64/stable"
        ]
    );
    assert!(
        !step.needs_root,
        "flatpak's system helper asks polkit for a system install"
    );
    assert_eq!(step.weight, 6);
    assert_eq!(step.command.cwd, None);
    assert!(
        step.command
            .env
            .contains(&("LC_ALL".to_string(), "C.UTF-8".to_string()))
    );
    assert!(
        step.command
            .env
            .contains(&("FLATPAK_FANCY_OUTPUT".to_string(), "0".to_string()))
    );
}

#[test]
fn plan_install_follows_the_installation_setting_and_the_remote() {
    // Flathub in both installations: the setting decides.
    let both = ScriptedRunner::present()
        .answers(&REMOTES_SYSTEM, &fixture("remotes-system.txt"))
        .answers(
            &REMOTES_USER,
            &format!(
                "flathub\thttps://dl.flathub.org/repo/\t\n{}",
                fixture("remotes-user.txt")
            ),
        );
    let mut user = Flatpak::with_parts("x86_64", both, flathub(), catalogue());
    user.installation = Installation::User;
    let steps = user
        .plan(&Op::Install {
            package: package_ref(FIREFOX),
        })
        .unwrap();
    assert_eq!(args(&steps[0])[3], "--user");

    // Flathub only in the system installation: a user preference cannot be
    // honoured, and the step goes where the remote is rather than failing.
    let mut user = source();
    user.installation = Installation::User;
    let steps = user
        .plan(&Op::Install {
            package: package_ref(FIREFOX),
        })
        .unwrap();
    assert_eq!(args(&steps[0])[3], "--system");

    // The same rule the other way: flathub-beta exists only in the user
    // installation, so the system preference gives way to it.
    let steps = source()
        .plan(&Op::Install {
            package: package_ref(GIMP_BETA),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(
        steps[0].title,
        "Installing GNU Image Manipulation Program from flathub-beta"
    );
    assert_eq!(
        args(&steps[0]),
        [
            "install",
            "-y",
            "--noninteractive",
            "--user",
            "flathub-beta",
            "app/org.gimp.GIMP/x86_64/beta"
        ]
    );
}

#[test]
fn plan_install_of_something_installed_has_nothing_to_do() {
    assert_eq!(
        source()
            .plan(&Op::Install {
                package: package_ref(GIMP)
            })
            .unwrap(),
        Vec::new()
    );
}

#[test]
fn plan_remove_targets_the_installation_it_lives_in() {
    let steps = source()
        .plan(&Op::Remove {
            package: package_ref(GIMP),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].title, "Removing GNU Image Manipulation Program");
    assert_eq!(
        args(&steps[0]),
        [
            "uninstall",
            "-y",
            "--noninteractive",
            "--system",
            "app/org.gimp.GIMP/x86_64/stable"
        ]
    );
    assert!(!steps[0].needs_root);
    assert_eq!(steps[0].weight, 3);

    let steps = source()
        .plan(&Op::Remove {
            package: package_ref(STEAM),
        })
        .unwrap();
    assert_eq!(steps[0].title, "Removing Steam");
    assert_eq!(args(&steps[0])[3], "--user");

    assert_eq!(
        source()
            .plan(&Op::Remove {
                package: package_ref(FIREFOX)
            })
            .unwrap(),
        Vec::new(),
        "not installed, nothing to remove"
    );
}

#[test]
fn plan_update_shapes() {
    let steps = source()
        .plan(&Op::Update {
            package: package_ref(GIMP),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].title, "Updating GNU Image Manipulation Program");
    assert_eq!(
        args(&steps[0]),
        [
            "update",
            "-y",
            "--noninteractive",
            "app/org.gimp.GIMP/x86_64/stable"
        ]
    );
    assert_eq!(steps[0].weight, 6);

    let steps = source()
        .plan(&Op::UpdateAll {
            source: SourceKind::Flatpak,
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].title, "Updating every Flatpak application");
    assert_eq!(args(&steps[0]), ["update", "-y", "--noninteractive"]);
    assert_eq!(steps[0].weight, 10);

    let steps = source()
        .plan(&Op::Refresh {
            source: SourceKind::Flatpak,
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].title, "Refreshing the Flatpak catalogues");
    assert_eq!(args(&steps[0]), ["update", "--appstream", "-y"]);
    assert!(steps.iter().all(|s| !s.needs_root));
}

#[test]
fn plan_ignores_another_source_s_operations() {
    let pacman = PackageRef {
        source: SourceKind::Pacman,
        id: "gimp".to_string(),
    };
    let source = source();
    assert_eq!(
        source
            .plan(&Op::Install {
                package: pacman.clone()
            })
            .unwrap(),
        Vec::new()
    );
    assert_eq!(
        source
            .plan(&Op::Remove {
                package: pacman.clone()
            })
            .unwrap(),
        Vec::new()
    );
    assert_eq!(
        source.plan(&Op::Update { package: pacman }).unwrap(),
        Vec::new()
    );
    assert_eq!(
        source
            .plan(&Op::UpdateAll {
                source: SourceKind::Snap
            })
            .unwrap(),
        Vec::new()
    );
    assert_eq!(
        source
            .plan(&Op::Refresh {
                source: SourceKind::Pacman
            })
            .unwrap(),
        Vec::new()
    );
}

#[test]
fn plan_titles_read_as_house_copy() {
    let ops = [
        Op::Install {
            package: package_ref(FIREFOX),
        },
        Op::Remove {
            package: package_ref(GIMP),
        },
        Op::Update {
            package: package_ref(GIMP),
        },
        Op::UpdateAll {
            source: SourceKind::Flatpak,
        },
        Op::Refresh {
            source: SourceKind::Flatpak,
        },
    ];
    for op in ops {
        for step in source().plan(&op).unwrap() {
            assert!(!step.title.contains('\u{2014}'), "{}", step.title);
            assert!(
                !step.title.ends_with('.'),
                "a title is a label, not a sentence: {}",
                step.title
            );
            assert!(
                step.title.chars().next().is_some_and(char::is_uppercase),
                "{}",
                step.title
            );
        }
    }
}

#[test]
fn progress_fixture_reads_back_as_fractions_of_the_whole() {
    let text = fixture("progress-install.txt");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 8);
    let read: Vec<Option<(f32, String)>> = lines.iter().map(|l| parse_progress(l)).collect();
    let expect = |i: usize, fraction: f32, message: &str| {
        let (f, m) = read[i]
            .clone()
            .unwrap_or_else(|| panic!("line {i} {:?} is progress", lines[i]));
        assert!(
            (f - fraction).abs() < 1e-4,
            "line {i}: {f} is not {fraction}"
        );
        assert_eq!(m, message, "line {i}");
    };
    expect(0, 0.0, "Installing 1 of 3");
    expect(1, 0.15, "Installing 1 of 3: 45%, 1.2 MB/s, 00:12 left");
    expect(2, 1.0 / 3.0, "Installing 2 of 3");
    expect(3, 2.0 / 3.0, "Installing 2 of 3: 100%, 3.4 MB/s");
    expect(4, 2.0 / 3.0, "Installing 3 of 3");
    expect(5, 2.0 / 3.0, "Installing 3 of 3: 0%");
    expect(
        6,
        2.67 / 3.0,
        "Installing 3 of 3: 67%, 980 bytes/s, 00:03 left",
    );
    assert_eq!(read[7], None, "the closing sentence belongs in the log");
    assert!(
        lines.iter().all(|l| parse_operation(l).is_none()),
        "none of these is a quiet line"
    );
    for window in read.iter().flatten().collect::<Vec<_>>().windows(2) {
        assert!(
            window[0].0 <= window[1].0,
            "the rail never runs backwards: {window:?}"
        );
    }
}

#[test]
fn quiet_fixture_reads_back_as_sentences() {
    let text = fixture("progress-noninteractive.txt");
    let read: Vec<Option<String>> = text.lines().map(parse_operation).collect();
    assert_eq!(
        read,
        [
            Some("Installing the org.gnome.Platform 50 runtime".to_string()),
            Some("Installing the org.gtk.Gtk3theme.adw-gtk3 3.22 runtime".to_string()),
            Some("Installing org.gimp.GIMP".to_string()),
            None,
        ]
    );
    assert!(
        text.lines().all(|l| parse_progress(l).is_none()),
        "no line here carries a fraction"
    );
}

#[test]
#[ignore = "reaches flathub.org"]
fn live_flathub_search_knows_gimp() {
    let api = LiveFlathub::new(Client::shared());
    let json = api.search("gimp").expect("flathub.org answers a search");
    let hits = parse_hits(&json);
    let gimp = hits.get("org.gimp.GIMP").expect("GIMP is on Flathub");
    assert_eq!(gimp.name.as_deref(), Some("GNU Image Manipulation Program"));
    assert!(gimp.installs_last_month.is_some_and(|n| n > 0), "{gimp:?}");
    assert!(gimp.updated_at.is_some());
    assert!(gimp.verified);
    assert!(
        gimp.icon
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
    );
    assert!(gimp.is_app);
}

#[test]
#[ignore = "reaches flathub.org"]
fn live_details_for_gimp_carry_flathub_s_facts() {
    let source = Flatpak::with_parts(
        "x86_64",
        runner(),
        LiveFlathub::new(Client::shared()),
        catalogue(),
    );
    let p = source.details(GIMP).expect("details for GIMP");
    let keys: Vec<&str> = p.facts.iter().map(|(k, _)| k.as_str()).collect();
    for key in [
        "Remote",
        "Branch",
        "Installation",
        "Runtime",
        "Download size",
        "Installed size",
        "Licence",
        "Verified",
    ] {
        assert!(keys.contains(&key), "{key} is missing from {keys:?}");
    }
    assert!(p.download_size.is_some_and(|n| n > 0));
    assert!(p.description.as_deref().is_some_and(|d| d.contains("<p>")));
    assert!(!p.screenshots.is_empty());
    assert!(
        p.screenshots
            .iter()
            .all(|s| matches!(s.image, Picture::Url(_)))
    );
    assert!(
        p.screenshots.iter().any(|s| s.thumbnail.is_some()),
        "a 624-wide rendition exists"
    );
    assert!(p.version.is_some());
}

#[test]
#[ignore = "reaches flathub.org"]
fn live_search_without_a_catalogue_comes_from_flathub() {
    let source = Flatpak::with_parts(
        "x86_64",
        runner(),
        LiveFlathub::new(Client::shared()),
        Catalogue::from_components(Vec::new()),
    );
    let found = source.search(&Query::new("gimp")).expect("search");
    assert!(!found.is_empty());
    assert_eq!(found[0].id, GIMP);
    assert!(found[0].popularity.is_some());
    assert!(
        found[0]
            .popularity_label
            .as_deref()
            .is_some_and(|l| l.ends_with(" installs last month"))
    );
    assert!(found[0].installed, "the list fixture says so");
    assert!(found.iter().all(|p| p.repo.as_deref() == Some("flathub")));
}
