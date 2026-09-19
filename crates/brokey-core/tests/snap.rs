//! The Snap source against snapd's answers, canned under
//! `fixtures/snap/`. snapd is not installed on the development machine, so
//! everything the source does is exercised through a transport that hands
//! back those files; the one `live_` test talks to the real socket where
//! there is one.

#![cfg(unix)]

use brokey_core::sources::linux::snap::{
    self, Presence, Response, ScriptedSnapStore, Snap, SnapInfo, SnapStore, Transport,
};
use brokey_core::transaction::allow::{Allowed, check_step};
use brokey_core::{
    Op, PackageKind, PackageRef, Picture, Query, Source, SourceKind, SourceSetup, SystemInfo,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/snap")
        .join(name)
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn arch() -> SystemInfo {
    brokey_core::system::from_os_release("ID=cachyos\nID_LIKE=arch\n")
}

fn debian() -> SystemInfo {
    brokey_core::system::from_os_release("ID=debian\n")
}

/// A snapd that answers from the fixtures. Unknown paths get snapd's 404
/// envelope, which is what the real one sends for a name it does not know.
struct Canned {
    presence: Presence,
    answers: HashMap<&'static str, &'static str>,
    /// Paths that fail as if the socket dropped, for the "store is down" cases.
    unreachable: Vec<&'static str>,
}

impl Canned {
    fn snapd() -> Canned {
        Canned {
            presence: Presence::Socket,
            answers: HashMap::from([
                ("/v2/system-info", "system-info.json"),
                ("/v2/snaps", "snaps.json"),
                ("/v2/snaps/firefox", "snaps-firefox.json"),
                ("/v2/find?q=firefox", "find-firefox.json"),
                ("/v2/find?q=visual%20studio%20code", "find-empty.json"),
                ("/v2/find?q=nonesuch", "find-empty.json"),
                ("/v2/find?name=firefox", "find-name-firefox.json"),
                ("/v2/find?name=code", "find-name-code.json"),
                ("/v2/find?name=chromium", "find-name-chromium.json"),
                ("/v2/find?select=refresh", "find-refresh.json"),
            ]),
            unreachable: Vec::new(),
        }
    }

    fn absent() -> Canned {
        Canned {
            presence: Presence::Absent,
            answers: HashMap::new(),
            unreachable: Vec::new(),
        }
    }

    fn with(mut self, path: &'static str, file: &'static str) -> Canned {
        self.answers.insert(path, file);
        self
    }

    fn without(mut self, path: &str) -> Canned {
        self.answers.remove(path);
        self
    }
}

impl Transport for Canned {
    fn presence(&self) -> Presence {
        self.presence
    }

    fn get(&self, path: &str) -> brokey_core::Result<Response> {
        if self.unreachable.contains(&path) {
            return Err(brokey_core::Error::from_source(
                SourceKind::Snap,
                "snapd closed the connection before its answer was complete. Try again.",
            ));
        }
        Ok(match self.answers.get(path) {
            Some(file) => Response {
                status: 200,
                body: fixture(file),
            },
            None => Response {
                status: 404,
                body: fixture("error-not-found.json"),
            },
        })
    }
}

/// A transport that records the paths asked of it, for the tests that
/// care how many round trips something costs.
struct Spy(Canned, std::sync::Arc<Mutex<Vec<String>>>);

impl Spy {
    fn on(canned: Canned) -> (Spy, std::sync::Arc<Mutex<Vec<String>>>) {
        let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
        (Spy(canned, asked.clone()), asked)
    }
}

impl Transport for Spy {
    fn presence(&self) -> Presence {
        self.0.presence()
    }
    fn get(&self, path: &str) -> brokey_core::Result<Response> {
        self.1.lock().unwrap().push(path.to_string());
        self.0.get(path)
    }
}

fn source(transport: Canned) -> Snap {
    Snap::with_transport(&arch(), Box::new(transport))
}

fn fact<'a>(facts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    facts
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn snap_ref(id: &str) -> PackageRef {
    PackageRef {
        source: SourceKind::Snap,
        id: id.to_string(),
    }
}

fn no_em_dash(s: &str) {
    assert!(!s.contains('\u{2014}'), "em dash in {s:?}");
}

// Status.

#[test]
fn without_snapd_the_status_is_searchable_and_offers_to_set_it_up() {
    let on_arch = Snap::with_transport(&arch(), Box::new(Canned::absent()))
        .with_aur_builds(true)
        .status();
    assert_eq!(on_arch.kind, SourceKind::Snap);
    assert!(!on_arch.available);
    assert!(on_arch.searchable);
    assert_eq!(
        on_arch.reason.as_deref(),
        Some(
            "snapd is not installed. The Snap Store is searched through its website, and installing from it sets snapd up first."
        )
    );
    assert_eq!(
        on_arch.setup,
        Some(SourceSetup {
            label: "Install snapd".to_string(),
            sentence: "Installs snapd and starts its service, then Snap applications can be installed and updated here. snapd comes from the AUR.".to_string(),
        })
    );
    assert_eq!(on_arch.detail, None);

    let on_debian = Snap::with_transport(&debian(), Box::new(Canned::absent())).status();
    assert_eq!(
        on_debian.setup.map(|s| s.sentence).as_deref(),
        Some(
            "Installs snapd and starts its service, then Snap applications can be installed and updated here."
        ),
        "the AUR is named only on Arch"
    );

    let no_builder = Snap::with_transport(&arch(), Box::new(Canned::absent()))
        .with_aur_builds(false)
        .status();
    assert!(no_builder.searchable);
    assert_eq!(no_builder.setup, None);
    assert_eq!(
        no_builder.reason.as_deref(),
        Some(
            "snapd is not installed. The Snap Store is searched through its website; install base-devel so snapd can be built from the AUR."
        )
    );
    for status in [&on_arch, &no_builder] {
        no_em_dash(status.reason.as_deref().unwrap());
    }
}

#[test]
fn with_the_program_but_no_socket_the_status_says_to_start_it() {
    let mut t = Canned::absent();
    t.presence = Presence::ProgramOnly;
    let status = source(t).status();
    assert!(!status.available);
    assert_eq!(
        status.reason.as_deref(),
        Some(
            "snapd is installed but not running. Start it with systemctl enable --now snapd.socket."
        )
    );
    assert!(status.searchable);
    assert_eq!(
        status.setup.map(|s| s.label).as_deref(),
        Some("Start snapd")
    );
}

fn store_json(name: &str) -> serde_json::Value {
    serde_json::from_slice(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// The public store with a search for gimp and a lookup of code.
fn store() -> ScriptedSnapStore {
    ScriptedSnapStore {
        find: Some(store_json("store-find-gimp.json")),
        info: HashMap::from([("code".to_string(), store_json("store-info-code.json"))]),
    }
}

fn without_snapd(system: &SystemInfo) -> Snap {
    Snap::with_transport(system, Box::new(Canned::absent()))
        .with_store(Box::new(store()))
        .with_aur_builds(true)
}

#[test]
fn without_snapd_search_asks_the_public_store() {
    let s = without_snapd(&arch());
    let found = s.search(&Query::new("gimp")).unwrap();
    let ids: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        ["gimp", "gimp-plugins-gmic", "openvino-ai-plugins-gimp"]
    );
    let gimp = &found[0];
    assert_eq!(gimp.source, SourceKind::Snap);
    assert_eq!(gimp.name, "GNU Image Manipulation Program");
    assert_eq!(gimp.kind, PackageKind::App);
    assert_eq!(
        gimp.summary.as_deref(),
        Some("High-end image creation and manipulation")
    );
    assert_eq!(gimp.developer.as_deref(), Some("GIMP team"));
    assert_eq!(gimp.version.as_deref(), Some("3.2.6"));
    assert_eq!(gimp.repo.as_deref(), Some("latest/stable"));
    assert!(matches!(&gimp.icon, Some(Picture::Url(u)) if u.starts_with("https://")));
    assert_eq!(
        gimp.screenshots.len(),
        5,
        "the banner leads, then four screenshots"
    );
    assert_eq!(gimp.categories, ["art-and-design"]);
    assert!(gimp.sandboxed);
    assert!(!gimp.installed);
    assert_eq!(fact(&gimp.facts, "Publisher"), Some("GIMP team (verified)"));
    assert_eq!(fact(&gimp.facts, "Revision"), Some("561"));
    assert_eq!(fact(&gimp.facts, "Confinement"), Some("strict"));

    let mut query = Query::new("gimp");
    query.limit = 1;
    assert_eq!(s.search(&query).unwrap().len(), 1);

    let offline = Snap::with_transport(&arch(), Box::new(Canned::absent()));
    let e = offline.search(&Query::new("gimp")).unwrap_err();
    assert_eq!(e.source_kind, Some(SourceKind::Snap));
    assert!(
        e.message.starts_with("The Snap Store did not answer"),
        "{}",
        e.message
    );
    assert!(e.message.ends_with('.'));
}

#[test]
fn without_snapd_details_and_confinement_come_from_the_public_store() {
    let s = without_snapd(&arch());
    let code = s.details("code").unwrap();
    assert_eq!(fact(&code.facts, "Confinement"), Some("classic"));
    assert!(!code.sandboxed);
    assert_eq!(code.repo.as_deref(), Some("latest/stable"));
    assert_eq!(
        s.details("nonesuch").unwrap_err().message,
        "Could not reach api.snapcraft.io."
    );

    // A plan for a classic snap found without snapd still says --classic.
    let fresh = without_snapd(&arch());
    let steps = fresh
        .plan(&Op::Install {
            package: snap_ref("code"),
        })
        .unwrap();
    assert_eq!(steps[0].command.args, ["install", "code", "--classic"]);

    let info = snap::parse_store_info(&store_json("store-info-code.json"), "amd64").unwrap();
    assert_eq!(info.name, "code");
    assert_eq!(info.channel, "stable");
    assert_eq!(snap::store_arch("x86_64"), "amd64");
    assert_eq!(snap::store_arch("aarch64"), "arm64");
    assert!(snap::parse_store_find(&serde_json::json!({})).is_empty());
}

#[test]
fn setting_snapd_up_installs_it_per_distribution_then_starts_it() {
    let installs = |s: &Snap| -> Vec<(SourceKind, String)> {
        s.setup()
            .unwrap()
            .ops
            .iter()
            .map(|op| match op {
                Op::Install { package } => (package.source, package.id.clone()),
                other => panic!("a setup installs, it does not {other:?}"),
            })
            .collect()
    };
    let programs = |s: &Snap| -> Vec<String> {
        s.setup()
            .unwrap()
            .steps
            .iter()
            .map(|step| {
                assert!(step.needs_root, "{}", step.title);
                assert_eq!(step.source, SourceKind::Snap);
                assert_eq!(
                    check_step(step, &Allowed::system()),
                    Ok(()),
                    "the helper allows {}",
                    step.title
                );
                format!("{} {}", step.command.program, step.command.args.join(" "))
            })
            .collect()
    };

    let on_arch = without_snapd(&arch());
    assert_eq!(installs(&on_arch), [(SourceKind::Aur, "snapd".to_string())]);
    assert_eq!(
        programs(&on_arch),
        [
            "systemctl enable --now snapd.socket",
            "ln -sfn /var/lib/snapd/snap /snap",
            "snap wait system seed.loaded",
        ]
    );
    assert_eq!(
        on_arch.setup().unwrap().notice,
        "snapd is not installed. It is built from the AUR and started."
    );

    let on_debian = without_snapd(&debian());
    assert_eq!(
        installs(&on_debian),
        [(SourceKind::Apt, "snapd".to_string())]
    );
    assert_eq!(
        programs(&on_debian),
        [
            "systemctl enable --now snapd.socket",
            "snap wait system seed.loaded",
        ],
        "Debian mounts snaps at /snap already"
    );
    assert_eq!(
        on_debian.setup().unwrap().notice,
        "snapd is not installed. It is installed and started."
    );

    let fedora = brokey_core::system::from_os_release("ID=fedora\n");
    let on_fedora = without_snapd(&fedora);
    assert_eq!(
        installs(&on_fedora),
        [(SourceKind::Dnf, "snapd".to_string())]
    );
    assert!(programs(&on_fedora).contains(&"ln -sfn /var/lib/snapd/snap /snap".to_string()));

    let no_builder = without_snapd(&arch()).with_aur_builds(false);
    assert!(no_builder.setup().is_none());
    let gentoo = brokey_core::system::from_os_release("ID=gentoo\n");
    assert!(without_snapd(&gentoo).setup().is_none());

    let mut stopped = Canned::absent();
    stopped.presence = Presence::ProgramOnly;
    let stopped = source(stopped);
    let setup = stopped.setup().unwrap();
    assert!(setup.ops.is_empty(), "snapd is installed already");
    assert_eq!(setup.notice, "snapd is not running. It is started.");

    assert!(source(Canned::snapd()).setup().is_none());
    assert_eq!(source(Canned::snapd()).status().setup, None);
}

#[test]
#[ignore = "needs the network"]
fn live_the_public_store_finds_gimp() {
    let store = snap::LiveSnapStore::new(brokey_core::http::Client::shared());
    let json = store.find("gimp").unwrap();
    let found = snap::parse_store_find(&json);
    assert!(found.iter().any(|s| s.name == "gimp"), "{json}");
    let info = store.info("code").unwrap();
    assert_eq!(
        snap::parse_store_info(&info, "amd64").unwrap().confinement,
        "classic"
    );
}

#[test]
fn with_snapd_the_status_carries_its_version() {
    let status = source(Canned::snapd()).status();
    assert!(status.available);
    assert_eq!(status.reason, None);
    assert_eq!(status.detail.as_deref(), Some("snapd 2.63.1"));
}

#[test]
fn when_snapd_does_not_answer_the_status_carries_the_reason() {
    let mut t = Canned::snapd();
    t.unreachable.push("/v2/system-info");
    let status = source(t).status();
    assert!(!status.available);
    assert_eq!(
        status.reason.as_deref(),
        Some("snapd closed the connection before its answer was complete. Try again.")
    );
}

// Installed.

#[test]
fn installed_snaps_map_every_field() {
    let (spy, asked) = Spy::on(Canned::snapd());
    let s = Snap::with_transport(&arch(), Box::new(spy));
    let installed = s.installed().unwrap();
    let names: Vec<&str> = installed.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(names, ["firefox", "core22", "snapd", "code", "hello"]);

    let firefox = &installed[0];
    assert_eq!(firefox.source, SourceKind::Snap);
    assert_eq!(firefox.name, "Firefox");
    assert_eq!(firefox.kind, PackageKind::App);
    assert_eq!(
        firefox.summary.as_deref(),
        Some("Mozilla Firefox web browser")
    );
    assert!(
        firefox
            .description
            .as_deref()
            .unwrap()
            .starts_with("Firefox is a free and open source")
    );
    assert_eq!(firefox.version.as_deref(), Some("140.0.4-1"));
    assert_eq!(firefox.installed_version.as_deref(), Some("140.0.4-1"));
    assert!(firefox.installed);
    assert_eq!(firefox.repo.as_deref(), Some("latest/stable"));
    assert_eq!(firefox.licence.as_deref(), Some("MPL-2.0"));
    assert_eq!(
        firefox.homepage.as_deref(),
        Some("https://snapcraft.io/firefox")
    );
    assert_eq!(firefox.developer.as_deref(), Some("Mozilla"));
    assert_eq!(firefox.updated, Some(1_788_092_527));
    assert_eq!(firefox.download_size, None);
    assert_eq!(firefox.installed_size, Some(274_702_336));
    assert_eq!(
        firefox.icon,
        Some(Picture::Url(
            "https://dashboard.snapcraft.io/site_media/appmedia/2021/12/firefox_logo.png".into()
        ))
    );
    assert_eq!(firefox.screenshots.len(), 1);
    assert_eq!(firefox.screenshots[0].width, Some(1920));
    assert_eq!(firefox.appstream_id.as_deref(), Some("org.mozilla.firefox"));
    assert!(firefox.sandboxed);
    assert!(!firefox.out_of_date);
    let keys: Vec<&str> = firefox.facts.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "Publisher",
            "Channel",
            "Revision",
            "Confinement",
            "Store page"
        ]
    );
    assert_eq!(
        fact(&firefox.facts, "Publisher"),
        Some("Mozilla (verified)")
    );
    assert_eq!(fact(&firefox.facts, "Channel"), Some("latest/stable"));
    assert_eq!(fact(&firefox.facts, "Revision"), Some("6338"));
    assert_eq!(fact(&firefox.facts, "Confinement"), Some("strict"));
    assert_eq!(
        fact(&firefox.facts, "Store page"),
        Some("https://snapcraft.io/firefox")
    );

    let core22 = &installed[1];
    assert_eq!(core22.name, "core22");
    assert_eq!(core22.kind, PackageKind::Runtime);
    assert_eq!(core22.icon, None);
    assert_eq!(core22.developer.as_deref(), Some("Canonical"));
    assert_eq!(core22.homepage, None);
    assert_eq!(fact(&core22.facts, "Store page"), None);

    assert_eq!(installed[2].kind, PackageKind::Runtime);

    let code = &installed[3];
    assert_eq!(code.name, "Visual Studio Code");
    assert_eq!(code.kind, PackageKind::App);
    assert!(!code.sandboxed, "a classic snap has no sandbox");
    assert_eq!(fact(&code.facts, "Confinement"), Some("classic"));
    assert_eq!(code.appstream_id.as_deref(), Some("com.visualstudio.code"));

    let hello = &installed[4];
    assert_eq!(
        hello.kind,
        PackageKind::Package,
        "a command-line snap is not an application"
    );
    assert_eq!(hello.name, "hello");
    assert!(hello.installed);
    assert_eq!(hello.icon, None);

    // The mount directory from system-info is asked for once, so an icon
    // file can be found on a distribution that does not use /snap.
    assert_eq!(s.installed().unwrap().len(), 5);
    let asked = asked.lock().unwrap().clone();
    assert_eq!(
        asked.iter().filter(|p| *p == "/v2/system-info").count(),
        1,
        "{asked:?}"
    );
}

#[test]
fn the_mount_directory_is_asked_for_once_even_when_it_is_the_first_guess() {
    // Ubuntu answers /snap, which is also the first guess; that must not
    // turn into a question on every call.
    let (spy, asked) = Spy::on(Canned::snapd().with("/v2/system-info", "system-info-ubuntu.json"));
    let s = Snap::with_transport(&debian(), Box::new(spy));
    assert_eq!(s.installed().unwrap().len(), 5);
    assert_eq!(s.installed().unwrap().len(), 5);
    assert_eq!(
        asked.lock().unwrap().as_slice(),
        ["/v2/snaps", "/v2/system-info", "/v2/snaps"]
    );
    assert_eq!(s.status().detail.as_deref(), Some("snapd 2.71+26.04"));

    // Directories handed in are the answer; snapd is not asked at all.
    let (spy, asked) = Spy::on(Canned::snapd());
    let s = Snap::with_transport(&arch(), Box::new(spy))
        .with_mount_dirs(vec![PathBuf::from("/nonexistent")]);
    assert_eq!(s.installed().unwrap().len(), 5);
    assert_eq!(asked.lock().unwrap().as_slice(), ["/v2/snaps"]);
}

#[test]
fn an_installed_snap_s_own_icon_file_is_found_under_the_mount_directory() {
    let dir = tempfile::tempdir().unwrap();
    let gui = dir.path().join("hello/current/meta/gui");
    std::fs::create_dir_all(&gui).unwrap();
    std::fs::write(gui.join("icon.svg"), "<svg/>").unwrap();
    let mut info = SnapInfo {
        name: "hello".into(),
        title: "Hello".into(),
        kind: "app".into(),
        icon: "/v2/icons/hello/icon".into(),
        ..SnapInfo::default()
    };
    let p = snap::to_package(&info, Some(&info), &[dir.path().to_path_buf()]);
    assert_eq!(p.icon, Some(Picture::File(gui.join("icon.svg"))));
    assert_eq!(p.kind, PackageKind::App);

    // Nowhere to find it: no icon, not a gap filled with a guess.
    let p = snap::to_package(&info, Some(&info), &[PathBuf::from("/nonexistent")]);
    assert_eq!(p.icon, None);

    // The store's picture wins when snapd knows it.
    info.media.push(snap::Media {
        kind: "icon".into(),
        url: "https://example.org/hello.png".into(),
        width: None,
        height: None,
    });
    let p = snap::to_package(&info, Some(&info), &[dir.path().to_path_buf()]);
    assert_eq!(
        p.icon,
        Some(Picture::Url("https://example.org/hello.png".into()))
    );
}

// Search.

#[test]
fn search_keeps_the_store_s_order_and_marks_what_is_installed() {
    let t = Canned::snapd();
    let s = Snap::with_transport(&arch(), Box::new(t));
    let found = s.search(&Query::new("firefox")).unwrap();
    let names: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(names, ["firefox", "chromium", "hello"]);

    let firefox = &found[0];
    assert!(firefox.installed);
    assert_eq!(firefox.installed_version.as_deref(), Some("140.0.4-1"));
    assert_eq!(firefox.version.as_deref(), Some("141.0-1"));
    assert_eq!(firefox.download_size, Some(262_156_288));
    assert_eq!(firefox.installed_size, Some(274_702_336));
    assert_eq!(firefox.updated, Some(1_788_092_527));
    assert_eq!(firefox.repo.as_deref(), Some("latest/stable"));
    assert_eq!(firefox.categories, ["productivity", "utilities"]);
    assert_eq!(
        firefox.screenshots.len(),
        3,
        "two screenshots and the banner; the video is not a picture"
    );
    assert!(
        firefox.screenshots[0].image
            == Picture::Url(
                "https://dashboard.snapcraft.io/site_media/appmedia/2021/12/firefox-banner.png"
                    .into()
            ),
        "the banner leads the rail"
    );
    assert_eq!(firefox.screenshots[0].height, Some(640));
    assert_eq!(
        fact(&firefox.facts, "Revision"),
        Some("6338"),
        "the installed revision, not the store's"
    );

    let chromium = &found[1];
    assert!(!chromium.installed);
    assert_eq!(chromium.installed_version, None);
    assert_eq!(chromium.updated, None);
    assert_eq!(chromium.kind, PackageKind::App);
    assert_eq!(
        chromium.repo.as_deref(),
        Some("latest/stable"),
        "a bare risk is written in full"
    );
    assert_eq!(fact(&chromium.facts, "Channel"), Some("latest/stable"));
    assert_eq!(fact(&chromium.facts, "Revision"), Some("3208"));
    assert_eq!(
        chromium.appstream_id.as_deref(),
        Some("org.chromium.Chromium")
    );
    assert!(chromium.sandboxed);

    let hello = &found[2];
    assert!(hello.installed);
    assert_eq!(hello.kind, PackageKind::Package);
}

#[test]
fn search_honours_the_limit() {
    let s = source(Canned::snapd());
    let mut q = Query::new("firefox");
    q.limit = 1;
    let found = s.search(&q).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, "firefox");
}

#[test]
fn search_encodes_the_text_and_asks_the_store_then_the_installed_list() {
    let (spy, asked) = Spy::on(Canned::snapd());
    let s = Snap::with_transport(&arch(), Box::new(spy));
    let found = s.search(&Query::new("visual studio code")).unwrap();
    assert!(found.is_empty());
    assert_eq!(
        asked.lock().unwrap().as_slice(),
        ["/v2/find?q=visual%20studio%20code", "/v2/snaps"]
    );

    asked.lock().unwrap().clear();
    assert!(s.search(&Query::new("   ")).unwrap().is_empty());
    assert!(
        asked.lock().unwrap().is_empty(),
        "an empty query asks nothing"
    );
}

#[test]
fn search_still_answers_when_the_installed_list_cannot_be_read() {
    let mut t = Canned::snapd();
    t.unreachable.push("/v2/snaps");
    let s = source(t);
    let found = s.search(&Query::new("firefox")).unwrap();
    assert_eq!(found.len(), 3);
    assert!(
        !found[0].installed,
        "unknown is reported as not installed, never guessed"
    );
}

#[test]
fn a_store_failure_is_a_sentence_not_a_stack_trace() {
    let t = Canned::snapd().with("/v2/find?q=firefox", "error-network.json");
    let e = source(t).search(&Query::new("firefox")).unwrap_err();
    assert_eq!(
        e.message,
        "The Snap Store did not answer in time. Try again."
    );
    assert_eq!(e.source_kind, Some(SourceKind::Snap));

    let t = Canned::snapd().without("/v2/find?q=firefox");
    let e = source(t).search(&Query::new("firefox")).unwrap_err();
    assert_eq!(e.message, "snapd answered: snap not found.");
}

// Updates.

#[test]
fn updates_pair_the_installed_version_with_the_new_one() {
    let s = source(Canned::snapd());
    let updates = s.updates().unwrap();
    assert_eq!(updates.len(), 2);
    let firefox = &updates[0];
    assert_eq!(firefox.package, snap_ref("firefox"));
    assert_eq!(firefox.name, "Firefox");
    assert_eq!(firefox.kind, PackageKind::App);
    assert_eq!(firefox.from.as_deref(), Some("140.0.4-1"));
    assert_eq!(firefox.to, "141.0-1");
    assert_eq!(firefox.download_size, Some(262_156_288));
    assert!(firefox.icon.is_some());
    assert_eq!(firefox.published, None);
    assert!(!firefox.is_self);
    let core22 = &updates[1];
    assert_eq!(core22.name, "core22");
    assert_eq!(core22.kind, PackageKind::Runtime);
    assert_eq!(core22.from.as_deref(), Some("20260812"));
    assert_eq!(core22.to, "20260903");
}

#[test]
fn no_updates_is_an_empty_list_without_a_second_call() {
    let t = Canned::snapd().with("/v2/find?select=refresh", "find-empty.json");
    let s = source(t);
    assert!(s.updates().unwrap().is_empty());
}

#[test]
fn updates_surface_the_store_s_network_failure() {
    let t = Canned::snapd().with("/v2/find?select=refresh", "error-network.json");
    let e = source(t).updates().unwrap_err();
    assert_eq!(
        e.message,
        "The Snap Store did not answer in time. Try again."
    );
}

// Details.

#[test]
fn details_merge_the_installed_record_with_the_store_s() {
    let s = source(Canned::snapd());
    let p = s.details("firefox").unwrap();
    assert!(p.installed);
    assert_eq!(
        p.version.as_deref(),
        Some("141.0-1"),
        "the store's newest in the channel"
    );
    assert_eq!(p.installed_version.as_deref(), Some("140.0.4-1"));
    assert_eq!(p.download_size, Some(262_156_288));
    assert_eq!(p.installed_size, Some(274_702_336));
    assert_eq!(
        p.screenshots.len(),
        3,
        "the store's pictures, not the cached two"
    );
    assert_eq!(p.updated, Some(1_788_092_527));
    assert_eq!(fact(&p.facts, "Revision"), Some("6338"));
    assert_eq!(fact(&p.facts, "Publisher"), Some("Mozilla (verified)"));
}

#[test]
fn details_of_a_snap_only_the_store_knows() {
    let s = source(Canned::snapd());
    let p = s.details("chromium").unwrap();
    assert!(!p.installed);
    assert_eq!(p.name, "Chromium");
    assert_eq!(p.kind, PackageKind::App);
    assert_eq!(p.version.as_deref(), Some("140.0.7339.80"));
    assert_eq!(p.homepage.as_deref(), Some("https://snapcraft.io/chromium"));
    assert_eq!(p.screenshots.len(), 1);
    assert_eq!(
        fact(&p.facts, "Store page"),
        Some("https://snapcraft.io/chromium")
    );
}

#[test]
fn details_of_an_installed_snap_stand_when_the_store_has_no_record_or_no_answer() {
    let t = Canned::snapd().without("/v2/find?name=firefox");
    let p = source(t).details("firefox").unwrap();
    assert!(p.installed);
    assert_eq!(p.version.as_deref(), Some("140.0.4-1"));

    let mut t = Canned::snapd();
    t.unreachable.push("/v2/find?name=firefox");
    let p = source(t).details("firefox").unwrap();
    assert!(p.installed);
    assert_eq!(p.name, "Firefox");
}

#[test]
fn details_of_an_unknown_name_say_so() {
    let e = source(Canned::snapd()).details("nonesuch").unwrap_err();
    assert_eq!(
        e.message,
        "nonesuch is not in the Snap Store and is not installed."
    );
    assert_eq!(e.source_kind, Some(SourceKind::Snap));
}

// Plans.

#[test]
fn installing_is_one_root_step_for_the_helper() {
    let s = source(Canned::snapd());
    let steps = s
        .plan(&Op::Install {
            package: snap_ref("firefox"),
        })
        .unwrap();
    assert_eq!(steps.len(), 1);
    let step = &steps[0];
    assert_eq!(step.source, SourceKind::Snap);
    assert_eq!(step.title, "Installing firefox from the Snap Store");
    assert_eq!(step.command.program, "snap");
    assert_eq!(step.command.args, ["install", "firefox"]);
    assert!(step.command.env.is_empty());
    assert_eq!(step.command.cwd, None);
    assert!(step.needs_root);
    assert_eq!(step.weight, 6);
}

#[test]
fn installing_a_classic_snap_adds_the_flag_learned_from_details() {
    let s = source(Canned::snapd());
    let p = s.details("code").unwrap();
    assert_eq!(fact(&p.facts, "Confinement"), Some("classic"));
    let steps = s
        .plan(&Op::Install {
            package: snap_ref("code"),
        })
        .unwrap();
    assert_eq!(steps[0].command.args, ["install", "code", "--classic"]);
}

#[test]
fn installing_a_classic_snap_from_cold_asks_the_store_once() {
    let (spy, asked) = Spy::on(Canned::snapd());
    let s = Snap::with_transport(&arch(), Box::new(spy));
    let op = Op::Install {
        package: snap_ref("code"),
    };
    assert_eq!(
        s.plan(&op).unwrap()[0].command.args,
        ["install", "code", "--classic"]
    );
    assert_eq!(asked.lock().unwrap().as_slice(), ["/v2/find?name=code"]);
    // Remembered: the second plan asks nothing.
    assert_eq!(
        s.plan(&op).unwrap()[0].command.args,
        ["install", "code", "--classic"]
    );
    assert_eq!(asked.lock().unwrap().len(), 1);
}

#[test]
fn installing_without_snapd_plans_strictly_and_asks_nothing() {
    let s = source(Canned::absent());
    let steps = s
        .plan(&Op::Install {
            package: snap_ref("code"),
        })
        .unwrap();
    assert_eq!(steps[0].command.args, ["install", "code"]);
}

#[test]
fn a_search_result_s_confinement_is_remembered_for_the_plan() {
    let t = Canned::snapd().with("/v2/find?q=code", "find-name-code.json");
    let s = source(t);
    let found = s.search(&Query::new("code")).unwrap();
    assert_eq!(fact(&found[0].facts, "Confinement"), Some("classic"));
    let steps = s
        .plan(&Op::Install {
            package: snap_ref("code"),
        })
        .unwrap();
    assert_eq!(steps[0].command.args, ["install", "code", "--classic"]);
}

#[test]
fn removing_updating_and_refreshing_have_their_own_steps() {
    let s = source(Canned::snapd());
    let remove = s
        .plan(&Op::Remove {
            package: snap_ref("firefox"),
        })
        .unwrap();
    assert_eq!(remove.len(), 1);
    assert_eq!(remove[0].command.args, ["remove", "firefox"]);
    assert_eq!(remove[0].title, "Removing firefox");
    assert!(remove[0].needs_root);

    let update = s
        .plan(&Op::Update {
            package: snap_ref("firefox"),
        })
        .unwrap();
    assert_eq!(update[0].command.args, ["refresh", "firefox"]);
    assert_eq!(update[0].title, "Updating firefox from the Snap Store");
    assert!(update[0].needs_root);

    let all = s
        .plan(&Op::UpdateAll {
            source: SourceKind::Snap,
        })
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].command.program, "snap");
    assert_eq!(all[0].command.args, ["refresh"]);
    assert_eq!(all[0].title, "Updating every snap");
    assert!(all[0].needs_root);

    let refresh = s
        .plan(&Op::Refresh {
            source: SourceKind::Snap,
        })
        .unwrap();
    assert!(refresh.is_empty(), "snapd keeps its own catalogue fresh");

    for step in remove.iter().chain(&update).chain(&all) {
        no_em_dash(&step.title);
    }
}

#[test]
fn another_source_s_package_is_refused() {
    let s = source(Canned::snapd());
    let e = s
        .plan(&Op::Install {
            package: PackageRef {
                source: SourceKind::Pacman,
                id: "firefox".into(),
            },
        })
        .unwrap_err();
    assert_eq!(e.message, "firefox is a pacman package, not a snap.");
    let e = s
        .plan(&Op::UpdateAll {
            source: SourceKind::Flatpak,
        })
        .unwrap_err();
    assert!(e.message.contains("not a Snap operation"), "{}", e.message);
}

// The HTTP parser on captures.

#[test]
fn a_content_length_capture_parses_to_the_json_it_carries() {
    let r = snap::parse_response(&fixture("content-length.http")).unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, fixture("system-info.json"));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["result"]["version"], "2.63.1");
}

#[test]
fn a_chunked_capture_parses_to_the_json_it_carries() {
    let r = snap::parse_response(&fixture("chunked.http")).unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(r.body, fixture("find-firefox.json"));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["result"][0]["name"], "firefox");
}

#[test]
fn every_fixture_is_the_shape_the_source_reads() {
    for name in [
        "system-info.json",
        "system-info-ubuntu.json",
        "snaps.json",
        "snaps-firefox.json",
        "find-firefox.json",
        "find-name-firefox.json",
        "find-name-code.json",
        "find-name-chromium.json",
        "find-refresh.json",
        "find-empty.json",
        "error-not-found.json",
        "error-network.json",
    ] {
        let v: serde_json::Value =
            serde_json::from_slice(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            v["type"] == "sync" || v["type"] == "error",
            "{name} has no snapd type"
        );
        assert!(v["status-code"].is_number(), "{name} has no status-code");
        assert!(!v["result"].is_null(), "{name} has no result");
    }
}

// The real thing, where there is one.

#[test]
#[ignore = "needs snapd on this machine"]
fn live_snapd_answers_system_info_and_lists_what_is_installed() {
    let transport = snap::SocketTransport::new();
    if transport.presence() != Presence::Socket {
        eprintln!(
            "{} is not here: snapd is not installed on this machine, nothing to talk to.",
            snap::SOCKET_PATH
        );
        return;
    }
    let s = Snap::new(
        &brokey_core::system::detect(),
        brokey_core::http::Client::shared(),
    );
    let status = s.status();
    assert!(status.available, "{:?}", status.reason);
    assert!(status.detail.as_deref().unwrap_or("").starts_with("snapd "));
    let installed = s.installed().unwrap();
    eprintln!("{} snaps installed", installed.len());
    let found = s.search(&Query::new("firefox")).unwrap();
    assert!(
        found.iter().any(|p| p.id == "firefox"),
        "the store did not return firefox"
    );
}
