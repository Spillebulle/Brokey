//! The grouper against fixtures: one test per rule, each with a case where
//! the rule fires and one where it must not. Nothing here touches the
//! network or the machine. `fixtures/group/*.json` are search results in the
//! shape the sources return, with the values the Arch catalogue, the AUR,
//! Flathub and the Snap Store gave on 2026-09-12; pacman's summary is its
//! package description, the others carry the AppStream one.

use brokey_core::group::{group, group_with, normalise_key};
use brokey_core::model::*;
use std::path::PathBuf;
use std::time::Instant;

fn pkg(source: SourceKind, id: &str, name: &str) -> Package {
    Package::new(source, id, name)
}

fn app(source: SourceKind, id: &str, name: &str) -> Package {
    let mut p = Package::new(source, id, name);
    p.kind = PackageKind::App;
    p
}

fn with_summary(mut p: Package, summary: &str) -> Package {
    p.summary = Some(summary.to_string());
    p
}

fn with_appstream(mut p: Package, id: &str) -> Package {
    p.appstream_id = Some(id.to_string());
    p
}

fn fixture(name: &str) -> Vec<Package> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "group",
        name,
    ]
    .iter()
    .collect();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Source, how it joined and how sure, for each edition in order.
fn editions(app: &App) -> Vec<(SourceKind, MatchedBy, f32)> {
    app.editions
        .iter()
        .map(|e| (e.package.source, e.matched_by, e.confidence))
        .collect()
}

fn by_key<'a>(apps: &'a [App], key: &str) -> &'a App {
    apps.iter().find(|a| a.key == key).unwrap_or_else(|| {
        let keys: Vec<&str> = apps.iter().map(|a| a.key.as_str()).collect();
        panic!("no app keyed {key}; keys are {keys:?}")
    })
}

fn names(apps: &[App]) -> Vec<&str> {
    apps.iter().map(|a| a.name.as_str()).collect()
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-4
}

// Rule 1: the AppStream id.

#[test]
fn appstream_id_joins_across_sources_case_insensitively() {
    let apps = group(
        vec![
            with_appstream(app(SourceKind::Pacman, "vlc", "VLC"), "org.videolan.vlc"),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/org.videolan.VLC/x86_64/stable",
                    "VLC",
                ),
                "org.videolan.VLC",
            ),
        ],
        "vlc",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(
        apps[0].key, "org.videolan.vlc",
        "the first edition's spelling is the key"
    );
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::AppStream, 1.0),
            (SourceKind::Flatpak, MatchedBy::AppStream, 1.0)
        ]
    );
}

#[test]
fn appstream_id_strips_a_trailing_desktop_before_comparing() {
    let apps = group(
        vec![
            with_appstream(
                app(SourceKind::Pacman, "steam", "Steam"),
                "com.valvesoftware.Steam.desktop",
            ),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/com.valvesoftware.Steam/x86_64/stable",
                    "Steam",
                ),
                "com.valvesoftware.Steam",
            ),
        ],
        "",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].key, "com.valvesoftware.Steam");
    assert!(
        apps[0]
            .editions
            .iter()
            .all(|e| e.matched_by == MatchedBy::AppStream)
    );
}

#[test]
fn different_appstream_ids_do_not_join_on_their_own() {
    let apps = group(
        vec![
            with_appstream(app(SourceKind::Pacman, "krita", "Krita"), "org.kde.krita"),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/org.kde.kdenlive/x86_64/stable",
                    "Kdenlive",
                ),
                "org.kde.kdenlive",
            ),
        ],
        "",
    );
    assert_eq!(apps.len(), 2);
}

#[test]
fn flatpak_ref_id_counts_as_its_appstream_id() {
    // The Flatpak source did not fill appstream_id; the ref says it anyway.
    let apps = group(
        vec![
            with_appstream(app(SourceKind::Pacman, "gimp", "GIMP"), "org.gimp.GIMP"),
            app(
                SourceKind::Flatpak,
                "flathub/app/org.gimp.GIMP/x86_64/stable",
                "GNU Image Manipulation Program",
            ),
        ],
        "",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::AppStream, 1.0),
            (SourceKind::Flatpak, MatchedBy::AppStream, 1.0)
        ]
    );

    // A runtime ref is not an application id, and a runtime is not an
    // edition of an application: nothing joins.
    let mut runtime = pkg(
        SourceKind::Flatpak,
        "flathub/runtime/org.freedesktop.Platform/x86_64/24.08",
        "Freedesktop Platform",
    );
    runtime.kind = PackageKind::Runtime;
    let platform = with_appstream(
        app(SourceKind::Pacman, "platform", "Platform"),
        "org.freedesktop.Platform",
    );
    let apps = group(vec![platform, runtime], "");
    assert_eq!(apps.len(), 2);
}

#[test]
fn same_appstream_id_within_one_source_joins() {
    // The Arch catalogue lists org.gnu.emacs under emacs and emacs-nox: one
    // application, two builds. A name match would have kept them apart.
    let apps = group(
        vec![
            with_appstream(app(SourceKind::Pacman, "emacs", "Emacs"), "org.gnu.emacs"),
            with_appstream(
                app(SourceKind::Pacman, "emacs-nox", "Emacs"),
                "org.gnu.emacs",
            ),
        ],
        "",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].editions.len(), 2);
    assert!(apps[0].editions.iter().all(|e| e.confidence == 1.0));
}

// Rule 2: the normalised name.

#[test]
fn name_key_joins_an_app_across_sources() {
    let apps = group(
        vec![
            with_summary(
                app(SourceKind::Pacman, "firefox", "Firefox"),
                "Fast, Private & Safe Web Browser",
            ),
            app(SourceKind::Snap, "firefox", "firefox"),
        ],
        "firefox",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::Name, 0.8),
            (SourceKind::Snap, MatchedBy::Name, 0.8)
        ]
    );
    assert_eq!(apps[0].key, "name:firefox");
}

#[test]
fn package_name_key_joins_when_the_display_name_differs() {
    // The catalogue names pacman's gimp "GNU Image Manipulation Program";
    // the Snap Store's gimp is still the same program, by package name.
    let apps = group(
        vec![
            app(SourceKind::Pacman, "gimp", "GNU Image Manipulation Program"),
            app(SourceKind::Snap, "gimp", "gimp"),
        ],
        "",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::Name, 0.8),
            (SourceKind::Snap, MatchedBy::Name, 0.8)
        ]
    );
    assert_eq!(apps[0].name, "GNU Image Manipulation Program");
    assert_eq!(apps[0].key, "name:gnuimagemanipulationprogram");

    // The package name is a key like any other: two plain packages that
    // share it and nothing else stay apart.
    let apps = group(
        vec![
            with_summary(
                pkg(SourceKind::Pacman, "dash", "Debian Almquist Shell"),
                "POSIX compliant shell that aims to be as small as possible",
            ),
            with_summary(
                pkg(SourceKind::Snap, "dash", "dash"),
                "Dash peer-to-peer network based digital currency",
            ),
        ],
        "",
    );
    assert_eq!(apps.len(), 2);
}

#[test]
fn name_key_joins_two_packages_when_their_summaries_agree() {
    let apps = group(
        vec![
            with_summary(
                pkg(SourceKind::Pacman, "yay", "yay"),
                "Yet another yogurt. Pacman wrapper and AUR helper written in go.",
            ),
            with_summary(
                pkg(SourceKind::Aur, "yay-bin", "yay-bin"),
                "Yet another yogurt. Pacman wrapper and AUR helper written in go. Pre-compiled.",
            ),
        ],
        "yay",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::Name, 0.6),
            (SourceKind::Aur, MatchedBy::Name, 0.6)
        ]
    );
    assert_eq!(
        apps[0].name, "yay",
        "the shortest name when no edition is an app"
    );
}

#[test]
fn name_key_alone_does_not_join_two_plain_packages() {
    let apps = group(
        vec![
            with_summary(
                pkg(SourceKind::Pacman, "dash", "dash"),
                "POSIX compliant shell that aims to be as small as possible",
            ),
            with_summary(
                pkg(SourceKind::Snap, "dash", "dash"),
                "Dash peer-to-peer network based digital currency",
            ),
        ],
        "dash",
    );
    assert_eq!(apps.len(), 2, "no shared significant word is no evidence");

    // Two shared significant words are enough; one is not.
    let apps = group(
        vec![
            with_summary(
                pkg(SourceKind::Pacman, "bluez", "bluez"),
                "Daemons for the bluetooth protocol stack",
            ),
            with_summary(
                pkg(SourceKind::Snap, "bluez", "bluez"),
                "BlueZ is the official bluetooth stack",
            ),
        ],
        "bluez",
    );
    assert_eq!(apps.len(), 1, "bluetooth and stack are two shared words");
    let apps = group(
        vec![
            with_summary(
                pkg(SourceKind::Pacman, "bluez", "bluez"),
                "Daemons for the bluetooth protocol",
            ),
            with_summary(
                pkg(SourceKind::Snap, "bluez", "bluez"),
                "BlueZ is the official bluetooth thing",
            ),
        ],
        "bluez",
    );
    assert_eq!(apps.len(), 2, "bluetooth alone is one shared word");
}

#[test]
fn same_source_never_joins_by_name() {
    let apps = group(
        vec![
            with_summary(
                app(SourceKind::Aur, "yay", "yay"),
                "Yet another yogurt. Pacman wrapper and AUR helper written in go.",
            ),
            with_summary(
                app(SourceKind::Aur, "yay-bin", "yay-bin"),
                "Yet another yogurt. Pacman wrapper and AUR helper written in go. Pre-compiled.",
            ),
            with_summary(
                app(SourceKind::Aur, "yay-git", "yay-git"),
                "Yet another yogurt. Pacman wrapper and AUR helper written in go. Git version.",
            ),
        ],
        "yay",
    );
    assert_eq!(apps.len(), 3);
    assert!(
        apps.iter()
            .all(|a| a.editions.len() == 1 && a.editions[0].matched_by == MatchedBy::Alone)
    );
}

#[test]
fn reverse_dns_name_reduces_to_its_last_segment() {
    // A Flatpak the source could not name: its name is its id. The ref's
    // AppStream id finds no twin; the name key still finds pacman's gimp.
    let apps = group(
        vec![
            app(SourceKind::Pacman, "gimp", "gimp"),
            app(
                SourceKind::Flatpak,
                "flathub/app/org.gimp.GIMP/x86_64/stable",
                "org.gimp.GIMP",
            ),
        ],
        "",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::Name, 0.8),
            (SourceKind::Flatpak, MatchedBy::Name, 0.8)
        ]
    );
    assert_eq!(
        apps[0].key, "org.gimp.GIMP",
        "an edition has an AppStream id, so that is the key"
    );
}

#[test]
fn generic_last_segment_keeps_the_vendor_in_the_key() {
    // org.telegram.desktop is telegram-desktop, not "desktop".
    let apps = group(
        vec![
            app(SourceKind::Pacman, "telegram-desktop", "Telegram Desktop"),
            app(
                SourceKind::Flatpak,
                "flathub/app/org.telegram.desktop/x86_64/stable",
                "org.telegram.desktop",
            ),
        ],
        "",
    );
    assert_eq!(apps.len(), 1, "{:?}", names(&apps));
    assert_eq!(apps[0].key, "org.telegram.desktop");

    // And "desktop" itself is not a key anything else can meet.
    let apps = group(
        vec![
            app(SourceKind::Snap, "desktop", "desktop"),
            app(
                SourceKind::Flatpak,
                "flathub/app/org.telegram.desktop/x86_64/stable",
                "org.telegram.desktop",
            ),
        ],
        "",
    );
    assert_eq!(apps.len(), 2);
}

#[test]
fn edition_suffix_joins_git_and_nightly_builds_but_not_a_two_letter_base() {
    let apps = group(
        vec![
            app(SourceKind::Pacman, "firefox", "Firefox"),
            pkg(SourceKind::Aur, "firefox-nightly", "firefox-nightly"),
            pkg(SourceKind::Aur, "firefox-git", "firefox-git"),
        ],
        "firefox",
    );
    assert_eq!(
        apps.len(),
        1,
        "two AUR builds each match pacman's, and so share its row"
    );
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::Name, 0.8),
            (SourceKind::Aur, MatchedBy::Name, 0.8),
            (SourceKind::Aur, MatchedBy::Name, 0.8)
        ]
    );

    let apps = group(
        vec![
            with_summary(
                pkg(SourceKind::Pacman, "go", "go"),
                "Core compiler tools for the Go programming language",
            ),
            with_summary(
                pkg(SourceKind::Aur, "go-bin", "go-bin"),
                "Go programming language compiler tools (binary release)",
            ),
        ],
        "go",
    );
    assert_eq!(
        apps.len(),
        2,
        "go-bin keeps its suffix: two letters are not a name"
    );
}

#[test]
fn different_reverse_dns_ids_never_join_by_name() {
    // GNOME's Files and elementary's Files: one name, two applications,
    // and both catalogues say so.
    let apps = group(
        vec![
            with_appstream(
                app(SourceKind::Pacman, "nautilus", "Files"),
                "org.gnome.Nautilus",
            ),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/io.elementary.files/x86_64/stable",
                    "Files",
                ),
                "io.elementary.files",
            ),
        ],
        "files",
    );
    assert_eq!(apps.len(), 2, "{:?}", names(&apps));

    // A legacy desktop-file id is not a vendor's claim: Arch's firefox is
    // "firefox", Flathub's is org.mozilla.firefox, and they are one thing.
    let apps = group(
        vec![
            with_appstream(app(SourceKind::Pacman, "firefox", "Firefox"), "firefox"),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/org.mozilla.firefox/x86_64/stable",
                    "Firefox",
                ),
                "org.mozilla.firefox",
            ),
        ],
        "firefox",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::Name, 0.8),
            (SourceKind::Flatpak, MatchedBy::Name, 0.8)
        ]
    );
    assert_eq!(apps[0].key, "firefox", "the first edition's AppStream id");
}

#[test]
fn a_package_without_an_id_cannot_bridge_two_different_ids() {
    // A Snap called "files" matches both by name. It joins the first, and
    // having joined it, may not join the second, or the two catalogues'
    // different applications would end up in one row.
    let apps = group(
        vec![
            with_appstream(
                app(SourceKind::Pacman, "nautilus", "Files"),
                "org.gnome.Nautilus",
            ),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/io.elementary.files/x86_64/stable",
                    "Files",
                ),
                "io.elementary.files",
            ),
            app(SourceKind::Snap, "files", "files"),
        ],
        "files",
    );
    assert_eq!(apps.len(), 2, "{:?}", names(&apps));
    let nautilus = by_key(&apps, "org.gnome.Nautilus");
    assert_eq!(
        editions(nautilus),
        vec![
            (SourceKind::Pacman, MatchedBy::Name, 0.8),
            (SourceKind::Snap, MatchedBy::Name, 0.8)
        ],
        "the Snap joined by name, and pacman's only companion is the Snap"
    );
    assert_eq!(by_key(&apps, "io.elementary.files").editions.len(), 1);
}

// Rule 3: false friends and the pairs that document the boundary.

#[test]
fn false_friends_never_join_even_with_an_app_on_one_side() {
    let pairs = vec![
        (
            app(SourceKind::Pacman, "wl-clipboard", "wl-clipboard"),
            pkg(SourceKind::Aur, "wl-clipboard-x11", "wl-clipboard-x11"),
        ),
        (
            pkg(SourceKind::Pacman, "libxkbcommon", "libxkbcommon"),
            app(SourceKind::Aur, "libxkbcommon-x11", "libxkbcommon-x11"),
        ),
        (
            app(SourceKind::Pacman, "ruby", "ruby"),
            pkg(SourceKind::Aur, "ruby-git", "ruby-git"),
        ),
    ];
    for (a, b) in pairs {
        assert_eq!(
            normalise_key(&a.name),
            normalise_key(&b.name),
            "{} / {} must share a key for this test to mean anything",
            a.name,
            b.name
        );
        let label = format!("{} / {}", a.name, b.name);
        let apps = group(vec![a, b], "");
        assert_eq!(apps.len(), 2, "{label} joined");
    }
}

#[test]
fn a_false_friend_still_joins_its_own_editions() {
    let apps = group(
        vec![
            app(SourceKind::Pacman, "wl-clipboard-x11", "wl-clipboard-x11"),
            app(
                SourceKind::Aur,
                "wl-clipboard-x11-git",
                "wl-clipboard-x11-git",
            ),
            app(SourceKind::Aur, "wl-clipboard-git", "wl-clipboard-git"),
        ],
        "",
    );
    assert_eq!(apps.len(), 2, "{:?}", names(&apps));
    assert_eq!(
        editions(by_key(&apps, "name:wlclipboardx11")),
        vec![
            (SourceKind::Pacman, MatchedBy::Name, 0.8),
            (SourceKind::Aur, MatchedBy::Name, 0.8)
        ]
    );
}

/// `-qt` and `-gtk` are not edition suffixes: on real data they name a
/// front-end, a binding or a module far more often than a build of the same
/// program, and every such name used to need a false-friend entry.
#[test]
fn toolkit_suffixes_name_different_programs_not_editions() {
    // Live search "neovim" on 2026-09-13: three programs, three rows.
    let apps = group(
        vec![
            with_summary(
                with_appstream(
                    app(SourceKind::Pacman, "neovim", "Neovim"),
                    "io.neovim.nvim",
                ),
                "Fork of Vim aiming to improve user experience, plugins, and GUIs",
            ),
            with_summary(
                with_appstream(app(SourceKind::Pacman, "neovim-qt", "Neovim-Qt"), "nvim-qt"),
                "Qt GUI for Neovim text editor",
            ),
            with_summary(
                pkg(SourceKind::Aur, "neovim-gtk", "neovim-gtk"),
                "GTK UI for Neovim written in Rust",
            ),
        ],
        "neovim",
    );
    assert_eq!(names(&apps), vec!["Neovim", "Neovim-Qt", "neovim-gtk"]);
    // Input method modules stay apart from the input method too, with no
    // list entry needed.
    let apps = group(
        vec![
            app(SourceKind::Pacman, "fcitx5", "Fcitx 5"),
            pkg(SourceKind::Aur, "fcitx5-qt", "fcitx5-qt"),
            pkg(SourceKind::Aur, "fcitx5-gtk", "fcitx5-gtk"),
        ],
        "",
    );
    assert_eq!(apps.len(), 3, "{:?}", names(&apps));
    // The real editions the suffix once joined still meet, through the
    // catalogue's id rather than the name.
    let apps = group(
        vec![
            with_appstream(
                app(SourceKind::Pacman, "wireshark-qt", "Wireshark"),
                "org.wireshark.Wireshark",
            ),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/org.wireshark.Wireshark/x86_64/stable",
                    "Wireshark",
                ),
                "org.wireshark.Wireshark",
            ),
            pkg(SourceKind::Aur, "wireshark-qt-git", "wireshark-qt-git"),
        ],
        "wireshark",
    );
    assert_eq!(apps.len(), 1, "{:?}", names(&apps));
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::AppStream, 1.0),
            (SourceKind::Flatpak, MatchedBy::AppStream, 1.0),
            (SourceKind::Aur, MatchedBy::Name, 0.8)
        ]
    );
}

/// `-beta` is a release channel, like `-canary`: two builds of one beta
/// channel belong together and apart from the stable row.
#[test]
fn a_beta_channel_is_its_own_row_and_a_git_build_is_not() {
    let apps = group(
        vec![
            with_appstream(
                app(SourceKind::Pacman, "signal-desktop", "Signal"),
                "org.signal.Signal",
            ),
            pkg(
                SourceKind::Aur,
                "signal-desktop-beta",
                "signal-desktop-beta",
            ),
            pkg(SourceKind::Aur, "signal-desktop-git", "signal-desktop-git"),
        ],
        "signal",
    );
    assert_eq!(apps.len(), 2, "{:?}", names(&apps));
    assert_eq!(
        editions(by_key(&apps, "org.signal.Signal")),
        vec![
            (SourceKind::Pacman, MatchedBy::Name, 0.8),
            (SourceKind::Aur, MatchedBy::Name, 0.8)
        ],
        "the git build joins, the beta channel does not"
    );
    assert_eq!(
        by_key(&apps, "name:signaldesktopbeta").editions[0].matched_by,
        MatchedBy::Alone
    );
}

// Rule 4: one source, two ids, two applications, whatever bridges them.

/// Live search "element" on 2026-09-13: the Arch catalogue gives `element`
/// (an audio plugin host) the desktop-file id `element` and
/// `element-desktop` (the Matrix client) `io.element.Element`; the AUR's
/// `element-git` borrows the first id and `element-desktop-git` the second,
/// and both answer to the name key `element`. Two rows, not one.
#[test]
fn one_source_giving_two_ids_is_two_rows_even_through_an_aur_bridge() {
    let packages = vec![
        with_summary(
            with_appstream(app(SourceKind::Pacman, "element", "Element"), "element"),
            "Audio Plugin Host and Modular Instrument",
        ),
        with_summary(
            with_appstream(
                app(SourceKind::Pacman, "element-desktop", "Element"),
                "io.element.Element",
            ),
            "Glossy Matrix collaboration client for desktop",
        ),
        with_summary(
            with_appstream(
                app(SourceKind::Aur, "element-git", "element-git"),
                "element",
            ),
            "Audio Plugin Host and Modular Instrument (git version)",
        ),
        with_summary(
            with_appstream(
                app(
                    SourceKind::Aur,
                    "element-desktop-git",
                    "element-desktop-git",
                ),
                "io.element.Element",
            ),
            "Glossy Matrix collaboration client for desktop (git version)",
        ),
    ];
    let apps = group(packages, "element");
    assert_eq!(apps.len(), 2, "{:?}", names(&apps));
    let matrix = by_key(&apps, "io.element.Element");
    assert_eq!(
        editions(matrix),
        vec![
            (SourceKind::Pacman, MatchedBy::AppStream, 1.0),
            (SourceKind::Aur, MatchedBy::AppStream, 1.0)
        ]
    );
    assert_eq!(
        matrix.summary.as_deref(),
        Some("Glossy Matrix collaboration client for desktop")
    );
    let host = by_key(&apps, "element");
    assert_eq!(
        editions(host),
        vec![
            (SourceKind::Pacman, MatchedBy::AppStream, 1.0),
            (SourceKind::Aur, MatchedBy::AppStream, 1.0)
        ]
    );
    assert_eq!(
        host.summary.as_deref(),
        Some("Audio Plugin Host and Modular Instrument")
    );
    // The rule is per source: another source's different id for the same
    // name is the ordinary name match it always was.
    let apps = group(
        vec![
            with_appstream(app(SourceKind::Pacman, "foo", "Foo"), "foo"),
            with_appstream(app(SourceKind::Snap, "foo", "Foo"), "org.example.Foo"),
        ],
        "foo",
    );
    assert_eq!(apps.len(), 1, "{:?}", names(&apps));
}

#[test]
fn different_keys_never_join() {
    let pairs = vec![
        (
            app(SourceKind::Pacman, "steam", "Steam"),
            pkg(SourceKind::Aur, "steam-devices", "steam-devices"),
        ),
        (
            app(SourceKind::Pacman, "steam", "Steam"),
            pkg(
                SourceKind::Aur,
                "steam-native-runtime",
                "steam-native-runtime",
            ),
        ),
        (
            app(SourceKind::Pacman, "gimp", "GIMP"),
            pkg(SourceKind::Aur, "gimp-plugin-gmic", "gimp-plugin-gmic"),
        ),
        (
            app(SourceKind::Pacman, "firefox", "Firefox"),
            app(
                SourceKind::Aur,
                "firefox-developer-edition",
                "firefox-developer-edition",
            ),
        ),
        (
            app(SourceKind::Pacman, "vlc", "VLC"),
            pkg(SourceKind::Snap, "vlc-plugin-x", "vlc-plugin-x"),
        ),
        (
            app(SourceKind::Pacman, "vlc", "VLC"),
            pkg(SourceKind::Aur, "vlc-plugins-all", "vlc-plugins-all"),
        ),
        (
            app(SourceKind::Pacman, "discord", "Discord"),
            app(SourceKind::Aur, "discord-canary", "discord-canary"),
        ),
    ];
    for (a, b) in pairs {
        let label = format!("{} / {}", a.name, b.name);
        let apps = group(vec![a, b], "");
        assert_eq!(apps.len(), 2, "{label} joined");
    }
}

#[test]
fn the_pairs_that_must_join() {
    // GIMP: catalogue id on pacman and Flathub, name on the AUR.
    let apps = group(
        vec![
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/org.gimp.GIMP/x86_64/stable",
                    "GNU Image Manipulation Program",
                ),
                "org.gimp.GIMP",
            ),
            with_appstream(
                app(SourceKind::Pacman, "gimp", "GNU Image Manipulation Program"),
                "org.gimp.GIMP",
            ),
            pkg(SourceKind::Aur, "gimp-git", "gimp-git"),
        ],
        "gimp",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(
        editions(&apps[0]),
        vec![
            (SourceKind::Pacman, MatchedBy::AppStream, 1.0),
            (SourceKind::Flatpak, MatchedBy::AppStream, 1.0),
            (SourceKind::Aur, MatchedBy::Name, 0.8)
        ]
    );

    // Steam: pacman and the Snap Store, name only.
    let apps = group(
        vec![
            app(SourceKind::Pacman, "steam", "Steam"),
            app(SourceKind::Snap, "steam", "steam"),
        ],
        "steam",
    );
    assert_eq!(apps.len(), 1);

    // Firefox and its nightly build.
    let apps = group(
        vec![
            app(SourceKind::Pacman, "firefox", "Firefox"),
            pkg(SourceKind::Aur, "firefox-nightly", "firefox-nightly"),
        ],
        "firefox",
    );
    assert_eq!(apps.len(), 1);

    // VLC: the Arch catalogue spells the id in lower case, Flathub does not.
    let apps = group(
        vec![
            with_appstream(app(SourceKind::Pacman, "vlc", "VLC"), "org.videolan.vlc"),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/org.videolan.VLC/x86_64/stable",
                    "VLC",
                ),
                "org.videolan.VLC",
            ),
        ],
        "vlc",
    );
    assert_eq!(apps.len(), 1);
}

#[test]
fn steam_from_four_sources_is_one_app_and_the_runtime_is_not() {
    let apps = group(
        vec![
            with_appstream(
                app(SourceKind::Pacman, "steam", "Steam"),
                "com.valvesoftware.Steam.desktop",
            ),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/com.valvesoftware.Steam/x86_64/stable",
                    "Steam",
                ),
                "com.valvesoftware.Steam",
            ),
            app(SourceKind::Snap, "steam", "steam"),
            pkg(
                SourceKind::Aur,
                "steam-native-runtime",
                "steam-native-runtime",
            ),
        ],
        "steam",
    );
    assert_eq!(apps.len(), 2, "{:?}", names(&apps));
    let steam = by_key(&apps, "com.valvesoftware.Steam");
    assert_eq!(steam.name, "Steam");
    assert_eq!(steam.kind, PackageKind::App);
    assert_eq!(
        editions(steam),
        vec![
            (SourceKind::Pacman, MatchedBy::AppStream, 1.0),
            (SourceKind::Flatpak, MatchedBy::AppStream, 1.0),
            (SourceKind::Snap, MatchedBy::Name, 0.8)
        ]
    );
    let runtime = by_key(&apps, "name:steamnativeruntime");
    assert_eq!(
        editions(runtime),
        vec![(SourceKind::Aur, MatchedBy::Alone, 1.0)]
    );
    assert_eq!(apps[0].key, steam.key, "the exact match sorts first");
}

/// An installed application as Add/Remove Programs gives it, and the same
/// application in the three Windows package managers. The registry's
/// edition is the record of what is on the disk, with no provenance and no
/// newer version; every manager's edition leads it.
fn obsidian_on_windows() -> Vec<Package> {
    let mut arp = app(SourceKind::Arp, "HKLM\\Obsidian", "Obsidian");
    arp.installed = true;
    arp.installed_version = Some("1.8.9".to_string());
    arp.developer = Some("Obsidian".to_string());
    vec![
        arp,
        app(SourceKind::Scoop, "extras/obsidian", "Obsidian"),
        app(SourceKind::Choco, "obsidian", "Obsidian"),
        app(SourceKind::Winget, "Obsidian.Obsidian", "Obsidian"),
    ]
}

#[test]
fn an_add_remove_entry_is_the_last_edition_behind_every_package_manager() {
    let apps = group(obsidian_on_windows(), "obsidian");
    assert_eq!(apps.len(), 1, "{:?}", names(&apps));
    let sources: Vec<SourceKind> = apps[0].editions.iter().map(|e| e.package.source).collect();
    assert_eq!(
        sources,
        [
            SourceKind::Winget,
            SourceKind::Choco,
            SourceKind::Scoop,
            SourceKind::Arp
        ]
    );
    assert!(apps[0].installed, "the registry's edition still says so");
    // Whatever order the sources answered in.
    let mut reversed = obsidian_on_windows();
    reversed.reverse();
    let again = group(reversed, "obsidian");
    assert_eq!(again[0].editions[0].package.source, SourceKind::Winget);
    assert_eq!(
        again[0].editions.last().unwrap().package.source,
        SourceKind::Arp
    );
}

#[test]
fn an_add_remove_entry_with_no_manager_edition_is_its_own_row_and_leads_it() {
    // Only the registry knows this one: it stays a row, and its edition is
    // the first because it is the only one.
    let alone = group(
        vec![
            obsidian_on_windows().remove(0),
            app(SourceKind::Winget, "Valve.Steam", "Steam"),
        ],
        "",
    );
    assert_eq!(names(&alone), ["Obsidian", "Steam"]);
    let obsidian = &alone[0];
    assert_eq!(
        editions(obsidian),
        vec![(SourceKind::Arp, MatchedBy::Alone, 1.0)]
    );
    assert!(obsidian.installed);
    // And the rank never joins anything: a different program in a manager
    // is a different row, whichever source ranks first.
    assert_eq!(alone[1].editions[0].package.source, SourceKind::Winget);
    assert_eq!(alone[1].editions.len(), 1);
}

// Rule 4: the app's fields.

#[test]
fn app_takes_key_name_and_kind_from_its_editions() {
    // A pacman source that names by package name beside a Flathub that
    // names properly: the row says "Steam", not "steam".
    let apps = group(
        vec![
            app(SourceKind::Pacman, "steam", "steam"),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/com.valvesoftware.Steam/x86_64/stable",
                    "Steam",
                ),
                "com.valvesoftware.Steam",
            ),
        ],
        "",
    );
    assert_eq!(apps[0].key, "com.valvesoftware.Steam");
    assert_eq!(apps[0].name, "Steam");
    assert_eq!(apps[0].kind, PackageKind::App);

    // A plain package beside an application: the application's name and kind.
    let apps = group(
        vec![
            pkg(SourceKind::Aur, "kate-git", "kate-git"),
            app(SourceKind::Pacman, "kate", "Kate"),
        ],
        "",
    );
    assert_eq!(apps[0].name, "Kate");
    assert_eq!(apps[0].kind, PackageKind::App);
    assert_eq!(apps[0].key, "name:kate");

    // No application anywhere: the shortest name, the first edition's kind.
    let mut runtime = with_summary(
        pkg(
            SourceKind::Flatpak,
            "flathub/runtime/org.example.Thing/x86_64/1",
            "thing-runtime",
        ),
        "The thing runtime for flatpak",
    );
    runtime.kind = PackageKind::Runtime;
    let apps = group(
        vec![
            with_summary(
                pkg(SourceKind::Aur, "thing-runtime-git", "thing-runtime-git"),
                "The thing runtime, from git, for flatpak",
            ),
            runtime,
        ],
        "",
    );
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name, "thing-runtime");
    assert_eq!(
        apps[0].kind,
        PackageKind::Runtime,
        "the first edition in source order is the Flatpak"
    );
}

#[test]
fn app_prefers_metadata_by_source_and_the_longest_summary() {
    let mut aur = with_summary(
        pkg(SourceKind::Aur, "foo-git", "foo-git"),
        "Foo, the frobnicator, built from git",
    );
    aur.icon = Some(Picture::Url("https://example.org/aur.png".into()));
    aur.developer = Some("AUR maintainer".into());
    aur.categories = vec!["Utility".into()];
    let mut snap = with_summary(app(SourceKind::Snap, "foo", "foo"), "Foo frobnicates");
    snap.icon = Some(Picture::Url("https://example.org/snap.png".into()));
    snap.developer = Some("Canonical".into());
    let mut apt = with_summary(app(SourceKind::Apt, "foo", "foo"), "Foo frobnicates things");
    apt.icon = Some(Picture::File("/usr/share/icons/foo.png".into()));
    apt.developer = Some("Debian".into());
    apt.categories = vec!["System".into()];
    let pacman = app(SourceKind::Pacman, "foo", "Foo");

    let apps = group(vec![aur, snap, apt, pacman], "");
    assert_eq!(apps.len(), 1);
    let foo = &apps[0];
    assert_eq!(
        foo.summary.as_deref(),
        Some("Foo, the frobnicator, built from git"),
        "the longest summary wins"
    );
    assert_eq!(
        foo.icon,
        Some(Picture::Url("https://example.org/snap.png".into())),
        "pacman has none; Snap outranks the AUR and apt"
    );
    assert_eq!(foo.developer.as_deref(), Some("Canonical"));
    assert_eq!(
        foo.categories,
        vec!["Utility".to_string()],
        "the AUR outranks apt for metadata"
    );

    // On a tie in summary length the preferred source keeps it.
    let apps = group(
        vec![
            with_summary(app(SourceKind::Aur, "bar-git", "bar-git"), "Bar from git"),
            with_summary(app(SourceKind::Pacman, "bar", "Bar"), "Bar (stable)"),
        ],
        "",
    );
    assert_eq!(apps[0].summary.as_deref(), Some("Bar (stable)"));
}

/// An AUR pkgdesc is longer than a catalogue summary and describes the
/// build, so when a catalogue describes any edition only those compete.
#[test]
fn app_summary_comes_from_a_catalogue_edition_when_there_is_one() {
    // Live search "firefox" on 2026-09-13, reduced.
    let apps = group(
        vec![
            with_summary(
                with_appstream(
                    app(SourceKind::Pacman, "firefox", "Firefox"),
                    "org.mozilla.firefox",
                ),
                "Fast, Private & Safe Web Browser",
            ),
            with_summary(
                with_appstream(
                    app(SourceKind::Aur, "firefox-bin", "firefox-bin"),
                    "org.mozilla.firefox",
                ),
                "Standalone web browser from mozilla.org - Static binaries from upstream",
            ),
            with_summary(
                app(SourceKind::Github, "mozilla/firefox", "firefox"),
                "The Firefox web browser, mirrored on GitHub with a long description",
            ),
        ],
        "firefox",
    );
    assert_eq!(apps.len(), 1, "{:?}", names(&apps));
    assert_eq!(
        apps[0].summary.as_deref(),
        Some("Fast, Private & Safe Web Browser"),
        "the AUR and GitHub summaries do not compete with the catalogue's"
    );
    // Two catalogue editions still compete on length: Flathub's summary
    // beats pacman's package description.
    let apps = group(
        vec![
            with_summary(
                with_appstream(app(SourceKind::Pacman, "vlc", "VLC"), "org.videolan.vlc"),
                "Multi-platform MPEG, VCD/DVD, and DivX player",
            ),
            with_summary(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/org.videolan.VLC/x86_64/stable",
                    "VLC",
                ),
                "VLC media player, the open-source multimedia player",
            ),
            with_summary(
                with_appstream(
                    app(SourceKind::Aur, "vlc-git", "vlc-git"),
                    "org.videolan.vlc",
                ),
                "Multi-platform MPEG, VCD/DVD, and DivX player (monolithic) (git version)",
            ),
        ],
        "vlc",
    );
    assert_eq!(
        apps[0].summary.as_deref(),
        Some("VLC media player, the open-source multimedia player")
    );
    // With no catalogue edition at all, the longest summary still wins.
    let apps = group(
        vec![
            with_summary(
                pkg(SourceKind::Pacman, "yay", "yay"),
                "Pacman wrapper and AUR helper written in go",
            ),
            with_summary(
                pkg(SourceKind::Aur, "yay-bin", "yay-bin"),
                "Yet another yogurt. Pacman wrapper and AUR helper written in go. Pre-compiled.",
            ),
        ],
        "yay",
    );
    assert_eq!(apps.len(), 1, "{:?}", names(&apps));
    assert!(
        apps[0]
            .summary
            .as_deref()
            .is_some_and(|s| s.starts_with("Yet another")),
        "{:?}",
        apps[0].summary
    );
}

#[test]
fn app_aggregates_installed_updated_and_popularity() {
    let mut a = app(SourceKind::Pacman, "foo", "Foo");
    a.updated = Some(100);
    a.popularity = Some(0.2);
    let mut b = app(SourceKind::Snap, "foo", "foo");
    b.installed = true;
    b.updated = Some(300);
    let mut c = pkg(SourceKind::Aur, "foo-bin", "foo-bin");
    c.updated = Some(200);
    c.popularity = Some(0.7);
    let apps = group(vec![a, b, c], "");
    assert_eq!(apps.len(), 1);
    assert!(apps[0].installed);
    assert_eq!(apps[0].updated, Some(300));
    assert_eq!(apps[0].popularity, Some(0.7));
}

#[test]
fn editions_are_listed_in_source_order() {
    let apps = group(
        vec![
            app(SourceKind::Github, "foo/foo", "foo"),
            app(SourceKind::Aur, "foo-bin", "foo-bin"),
            app(SourceKind::Snap, "foo", "foo"),
            app(
                SourceKind::Flatpak,
                "flathub/app/org.example.foo/x86_64/stable",
                "foo",
            ),
            app(SourceKind::Dnf, "foo", "foo"),
            app(SourceKind::Apt, "foo", "foo"),
            app(SourceKind::Pacman, "foo", "foo"),
        ],
        "",
    );
    assert_eq!(apps.len(), 1);
    let order: Vec<SourceKind> = apps[0].editions.iter().map(|e| e.package.source).collect();
    assert_eq!(
        order,
        vec![
            SourceKind::Pacman,
            SourceKind::Apt,
            SourceKind::Dnf,
            SourceKind::Flatpak,
            SourceKind::Snap,
            SourceKind::Aur,
            SourceKind::Github
        ]
    );
}

#[test]
fn key_is_the_appstream_id_else_the_normalised_name() {
    let apps = group(
        vec![
            with_appstream(app(SourceKind::Pacman, "gimp", "GIMP"), "org.gimp.GIMP"),
            app(SourceKind::Snap, "inkscape", "inkscape"),
            app(
                SourceKind::Flatpak,
                "flathub/app/org.kde.krita/x86_64/stable",
                "Krita",
            ),
        ],
        "",
    );
    assert_eq!(by_key(&apps, "org.gimp.GIMP").name, "GIMP");
    assert_eq!(by_key(&apps, "name:inkscape").name, "inkscape");
    assert_eq!(
        by_key(&apps, "org.kde.krita").name,
        "Krita",
        "a lone Flatpak is keyed by its ref's id"
    );
}

#[test]
fn keys_are_unique_within_a_result() {
    let apps = group(
        vec![
            pkg(SourceKind::Aur, "yay", "yay"),
            pkg(SourceKind::Aur, "yay-bin", "yay-bin"),
            pkg(SourceKind::Aur, "yay-git", "yay-git"),
            pkg(SourceKind::Aur, "paru", "paru"),
        ],
        "",
    );
    let mut keys: Vec<&str> = apps.iter().map(|a| a.key.as_str()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), 4, "{keys:?}");
    assert!(
        keys.contains(&"name:paru"),
        "an unambiguous key stays short"
    );
    assert!(keys.contains(&"name:yay:aur:yay"));
    assert!(keys.contains(&"name:yay:aur:yay-bin"));
}

// Rule 5: relevance and order.

#[test]
fn relevance_tiers_in_order() {
    let apps = group(
        vec![
            with_summary(pkg(SourceKind::Pacman, "vim", "vim"), "A text editor"),
            with_summary(
                pkg(SourceKind::Pacman, "lutris", "lutris"),
                "Games from GOG, Humble and unsteamed stores",
            ),
            with_summary(
                pkg(SourceKind::Pacman, "proton", "proton"),
                "Runs Steamworks games",
            ),
            pkg(SourceKind::Pacman, "opensteamworks", "opensteamworks"),
            pkg(SourceKind::Pacman, "steamcmd", "steamcmd"),
            pkg(SourceKind::Pacman, "steam", "steam"),
        ],
        "steam",
    );
    assert_eq!(
        names(&apps),
        vec![
            "steam",
            "steamcmd",
            "opensteamworks",
            "proton",
            "lutris",
            "vim"
        ]
    );
    let scores: Vec<f32> = apps.iter().map(|a| a.relevance).collect();
    for (score, want) in scores.iter().zip([1.0, 0.9, 0.75, 0.6, 0.4, 0.2]) {
        assert!(close(*score, want), "{scores:?}");
    }
}

#[test]
fn relevance_matches_by_normalised_key_and_any_edition_name() {
    let steam = || {
        with_appstream(
            app(SourceKind::Pacman, "steam", "Steam"),
            "com.valvesoftware.Steam",
        )
    };
    let apps = group(
        vec![steam(), pkg(SourceKind::Aur, "steam-git", "steam-git")],
        "steam-git",
    );
    assert!(
        close(apps[0].relevance, 1.0),
        "an edition is named exactly that"
    );
    let apps = group(vec![steam()], "com.valvesoftware.steam");
    assert!(
        close(apps[0].relevance, 1.0),
        "the query normalises to the same key"
    );
    let apps = group(vec![steam()], "valvesoftware");
    assert!(
        close(apps[0].relevance, 0.75 + 0.05),
        "the AppStream id contains the query"
    );

    // The "name:" prefix of a key is bookkeeping, not something to match.
    let apps = group(vec![pkg(SourceKind::Pacman, "vim", "vim")], "name");
    assert!(close(apps[0].relevance, 0.2), "{}", apps[0].relevance);
}

#[test]
fn relevance_bonuses_for_app_installed_and_popularity() {
    let mut installed = pkg(SourceKind::Aur, "steam", "steam");
    installed.installed = true;
    installed.popularity = Some(0.5);
    let apps = group(
        vec![pkg(SourceKind::Pacman, "steam", "steam"), installed],
        "steam",
    );
    assert_eq!(
        apps.len(),
        2,
        "two plain packages with no summaries stay apart"
    );
    assert_eq!(
        apps[0].editions[0].package.source,
        SourceKind::Aur,
        "installed and popular sorts first"
    );
    assert!(
        apps.iter().all(|a| a.relevance <= 1.0),
        "the page gets the documented range"
    );

    let apps = group(
        vec![
            app(SourceKind::Pacman, "steamtinker", "steamtinker"),
            pkg(SourceKind::Pacman, "steamcmd", "steamcmd"),
        ],
        "steam",
    );
    assert_eq!(
        names(&apps),
        vec!["steamtinker", "steamcmd"],
        "an application outranks a package in the same tier"
    );
    assert!(close(apps[0].relevance, 0.95));
}

#[test]
fn sort_is_relevance_then_app_first_then_name() {
    let apps = group(
        vec![
            pkg(SourceKind::Pacman, "steam-devices", "steam-devices"),
            app(SourceKind::Pacman, "steam-tui", "steam-tui"),
            pkg(SourceKind::Pacman, "steamcmd", "steamcmd"),
            app(SourceKind::Pacman, "steam-acf", "steam-acf"),
        ],
        "steam",
    );
    assert_eq!(
        names(&apps),
        vec!["steam-acf", "steam-tui", "steam-devices", "steamcmd"]
    );
}

/// Live search "docker" on 2026-09-13: the catalogue names the Cockpit
/// add-on `cockpit-docker` "Docker", so it ties with pacman's `docker` on
/// relevance and kind. The package the source itself calls by the query
/// comes first, in both sorts.
#[test]
fn a_package_the_source_names_after_the_query_wins_a_tie() {
    let packages = || {
        vec![
            with_summary(
                with_appstream(
                    pkg(SourceKind::Pacman, "cockpit-docker", "Docker"),
                    "me.chabad360.docker",
                ),
                "Cockpit UI for docker containers",
            ),
            with_summary(
                pkg(SourceKind::Pacman, "docker", "docker"),
                "Pack, ship and run any application as a lightweight container",
            ),
        ]
    };
    let apps = group(packages(), "docker");
    assert_eq!(names(&apps), vec!["docker", "Docker"]);
    assert!(close(apps[0].relevance, apps[1].relevance), "a tie");
    // The tie-break comes after relevance and kind: an application still
    // outranks a package the source names after the query.
    let mut with_app = packages();
    with_app[0].kind = PackageKind::App;
    let apps = group(with_app, "docker");
    assert_eq!(names(&apps), vec!["Docker", "docker"]);
    // The split path sorts the rows again, by the same rule.
    let apps = group_with(packages(), "docker", &["pacman:cockpit-docker".to_string()]);
    assert_eq!(names(&apps), vec!["docker", "Docker"]);
    let apps = group_with(
        packages(),
        "  Docker ",
        &["pacman:cockpit-docker".to_string()],
    );
    assert_eq!(
        names(&apps),
        vec!["docker", "Docker"],
        "the query is trimmed and lower-cased"
    );
}

#[test]
fn empty_query_sorts_by_name_with_zero_relevance() {
    let apps = group(
        vec![
            with_summary(app(SourceKind::Pacman, "vlc", "VLC"), "Media player"),
            app(SourceKind::Pacman, "gimp", "GIMP"),
            app(SourceKind::Pacman, "audacity", "Audacity"),
        ],
        "   ",
    );
    assert_eq!(names(&apps), vec!["Audacity", "GIMP", "VLC"]);
    assert!(apps.iter().all(|a| a.relevance == 0.0));
}

#[test]
fn single_package_is_alone_with_full_confidence() {
    let apps = group(vec![app(SourceKind::Pacman, "gimp", "GIMP")], "gimp");
    assert_eq!(
        editions(&apps[0]),
        vec![(SourceKind::Pacman, MatchedBy::Alone, 1.0)]
    );
}

#[test]
fn empty_input_gives_empty_output() {
    assert!(group(Vec::new(), "steam").is_empty());
}

#[test]
fn result_is_the_same_whatever_the_input_order() {
    // Joins can be refused, so the order pairs are tried in matters; it must
    // come from the input, never from a hash map's whim.
    let packages = || {
        vec![
            with_appstream(
                app(SourceKind::Pacman, "nautilus", "Files"),
                "org.gnome.Nautilus",
            ),
            with_appstream(
                app(
                    SourceKind::Flatpak,
                    "flathub/app/io.elementary.files/x86_64/stable",
                    "Files",
                ),
                "io.elementary.files",
            ),
            app(SourceKind::Snap, "files", "files"),
            app(SourceKind::Apt, "nautilus", "Files"),
            pkg(SourceKind::Aur, "nautilus-git", "nautilus-git"),
        ]
    };
    let first = group(packages(), "files");
    for _ in 0..20 {
        assert_eq!(group(packages(), "files"), first);
    }
}

// Rule 6: cost.

#[test]
fn two_thousand_packages_group_quickly() {
    let mut packages = Vec::with_capacity(2000);
    for i in 0..500 {
        let mut p = app(SourceKind::Pacman, &format!("app{i}"), &format!("App {i}"));
        p.appstream_id = Some(format!("org.example.App{i}"));
        p.summary = Some(format!("Application number {i} for frobnicating widgets"));
        packages.push(p);
        let mut f = app(
            SourceKind::Flatpak,
            &format!("flathub/app/org.example.App{i}/x86_64/stable"),
            &format!("App {i}"),
        );
        f.summary = Some(format!("Application number {i} for frobnicating widgets"));
        packages.push(f);
        packages.push(with_summary(
            app(SourceKind::Snap, &format!("app{i}"), &format!("app{i}")),
            &format!("Application number {i}"),
        ));
        packages.push(with_summary(
            pkg(
                SourceKind::Aur,
                &format!("app{i}-git"),
                &format!("app{i}-git"),
            ),
            &format!("Application number {i}, git version"),
        ));
    }
    assert_eq!(packages.len(), 2000);
    let started = Instant::now();
    let apps = group(packages, "app7");
    let took = started.elapsed();
    assert_eq!(apps.len(), 500);
    assert!(
        apps.iter().all(|a| a.editions.len() == 4),
        "every app has four editions"
    );
    assert_eq!(apps[0].name, "App 7");
    assert!(
        took.as_millis() < 500,
        "grouping 2000 packages took {took:?}"
    );
}

// Fixtures: search results as the sources return them.

#[test]
fn steam_fixture_groups_as_the_page_expects() {
    let apps = group(fixture("steam.json"), "steam");
    assert_eq!(apps.len(), 3, "{:?}", names(&apps));

    let steam = &apps[0];
    assert_eq!(steam.key, "com.valvesoftware.Steam");
    assert_eq!(steam.name, "Steam");
    assert_eq!(steam.kind, PackageKind::App);
    assert_eq!(
        editions(steam),
        vec![
            (SourceKind::Pacman, MatchedBy::AppStream, 1.0),
            (SourceKind::Flatpak, MatchedBy::AppStream, 1.0),
            (SourceKind::Snap, MatchedBy::Name, 0.8)
        ]
    );
    assert_eq!(
        steam.summary.as_deref(),
        Some("Launcher for the Steam software distribution service"),
        "the catalogue's summary is longer than pacman's package description"
    );
    let icon_path = match &steam.icon {
        Some(Picture::File(p)) => p.to_string_lossy().into_owned(),
        other => panic!("expected the catalogue's icon file, got {other:?}"),
    };
    assert!(
        icon_path.ends_with("archlinux-arch-multilib/128x128/steam_steam.png"),
        "{icon_path}"
    );
    assert_eq!(steam.developer.as_deref(), Some("Valve Corporation"));
    assert_eq!(
        steam.categories,
        vec!["Game".to_string(), "PackageManager".to_string()]
    );
    assert!(steam.installed);
    assert_eq!(steam.updated, Some(1785004799));
    assert_eq!(steam.popularity, Some(0.92));
    assert!(close(steam.relevance, 1.0));

    assert_eq!(
        names(&apps)[1..],
        ["steamcmd", "steam-native-runtime"],
        "both start with the query; votes break the tie"
    );
    assert_eq!(
        by_key(&apps, "name:steamcmd").editions[0].matched_by,
        MatchedBy::Alone
    );
    assert_eq!(
        by_key(&apps, "name:steamnativeruntime").editions[0].matched_by,
        MatchedBy::Alone
    );
}

#[test]
fn gimp_fixture_groups_as_the_page_expects() {
    let apps = group(fixture("gimp.json"), "gimp");
    assert_eq!(apps.len(), 2, "{:?}", names(&apps));

    let gimp = &apps[0];
    assert_eq!(gimp.key, "org.gimp.GIMP");
    assert_eq!(gimp.name, "GNU Image Manipulation Program");
    assert_eq!(
        editions(gimp),
        vec![
            (SourceKind::Pacman, MatchedBy::AppStream, 1.0),
            (SourceKind::Flatpak, MatchedBy::AppStream, 1.0),
            (SourceKind::Snap, MatchedBy::Name, 0.8),
            (SourceKind::Aur, MatchedBy::Name, 0.8)
        ]
    );
    assert_eq!(
        gimp.summary.as_deref(),
        Some("High-end image creation and manipulation")
    );
    assert_eq!(gimp.developer.as_deref(), Some("The GIMP team"));
    assert_eq!(gimp.popularity, Some(0.88));
    assert!(
        close(gimp.relevance, 1.0),
        "the package name is exactly the query"
    );

    let gmic = &apps[1];
    assert_eq!(gmic.key, "name:gimpplugingmic");
    assert_eq!(gmic.kind, PackageKind::Package);
    assert_eq!(
        editions(gmic),
        vec![(SourceKind::Pacman, MatchedBy::Alone, 1.0)]
    );
    assert!(close(gmic.relevance, 0.9));
}

// Windows: the Add/Remove Programs uninstall key.

/// An installed package on Windows the way the two sources give it: winget's
/// edition named by the catalogue, the registry's by its `DisplayName`, both
/// naming the uninstall key. The values are the development machine's.
fn arp_entry(key: &str, name: &str, version: &str) -> Package {
    let mut p = app(SourceKind::Arp, key, name);
    p.installed = true;
    p.installed_version = Some(version.to_string());
    p.facts.push(("Registry key".to_string(), key.to_string()));
    p
}

fn winget_installed(id: &str, name: &str, version: &str, key: Option<&str>) -> Package {
    let mut p = app(SourceKind::Winget, id, name);
    p.installed = true;
    p.installed_version = Some(version.to_string());
    if let Some(key) = key {
        p.facts.push(("Registry key".to_string(), key.to_string()));
    }
    p
}

/// The rule fires: `Advanced IP Scanner` and `Advanced IP Scanner 2.5.1`
/// share no name key, and before this rule they were two rows on the
/// Installed page. The uninstall key both name is exact, so the join says
/// so with full confidence rather than "matched by name".
#[test]
fn a_shared_uninstall_key_joins_editions_whose_names_differ() {
    let key = r"HKLM32\{8A6B3B7E-3E2D-4E5B-9C47-5F2B8A2C1D10}";
    let apps = group(
        vec![
            arp_entry(key, "Advanced IP Scanner 2.5.1", "2.5.4594.1"),
            winget_installed(
                "Famatech.AdvancedIPScanner",
                "Advanced IP Scanner",
                "2.5.4594.1",
                Some(key),
            ),
        ],
        "",
    );
    assert_eq!(names(&apps), vec!["Advanced IP Scanner"]);
    let mut got = editions(&apps[0]);
    got.sort_by_key(|(s, _, _)| s.id());
    assert_eq!(
        got,
        vec![
            (SourceKind::Arp, MatchedBy::Registry, 1.0),
            (SourceKind::Winget, MatchedBy::Registry, 1.0),
        ]
    );
}

/// It must not fire across two different keys, even with the same name:
/// two versions of Beeper installed side by side are two entries, and each
/// winget package joins only the entry it was matched to.
#[test]
fn different_uninstall_keys_stay_apart_whatever_the_names() {
    let (old, new) = (r"HKCU\beeper-3", r"HKCU\beeper-4");
    let apps = group(
        vec![
            arp_entry(old, "Beeper 3.111.7", "3.111.7"),
            arp_entry(new, "Beeper 4.2.957", "4.2.957"),
            winget_installed("Beeper.Beeper.3", "Beeper", "3.111.7", Some(old)),
            winget_installed("Beeper.Beeper", "Beeper", "4.2.957", Some(new)),
        ],
        "",
    );
    assert_eq!(apps.len(), 2, "{:?}", names(&apps));
    for a in &apps {
        let keys: Vec<&str> = a
            .editions
            .iter()
            .flat_map(|e| e.package.facts.iter())
            .filter(|(k, _)| k == "Registry key")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert_eq!(keys[0], keys[1], "one entry per row: {keys:?}");
    }
}

/// Nor does it fire for a package with no key, or for an entry nothing was
/// matched to: those fall to the name rule as before, and `7-Zip` and
/// `7-Zip 26.03 (x64)` share no name key.
#[test]
fn without_a_shared_key_nothing_joins_on_the_registry() {
    let apps = group(
        vec![
            arp_entry(r"HKLM\7-Zip", "7-Zip 26.03 (x64)", "26.03"),
            winget_installed("7zip.7zip", "7-Zip", "26.03", None),
        ],
        "",
    );
    assert_eq!(apps.len(), 2);
    assert!(
        apps.iter()
            .all(|a| a.editions.len() == 1 && a.editions[0].matched_by == MatchedBy::Alone)
    );
}

/// One source never lists one entry twice, and if a source ever repeats a key
/// across two of its own packages the registry rule does not join them: the
/// rule is across sources only, as the name rule is.
#[test]
fn one_source_naming_one_key_twice_is_not_joined_by_it() {
    let key = r"HKLM\Something";
    let apps = group(
        vec![
            winget_installed("Vendor.One", "One", "1.0", Some(key)),
            winget_installed("Vendor.Two", "Two", "1.0", Some(key)),
        ],
        "",
    );
    assert_eq!(apps.len(), 2);
}
