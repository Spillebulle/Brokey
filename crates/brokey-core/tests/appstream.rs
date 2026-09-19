//! The AppStream reader against a cut of the Arch catalogue and a hand-written
//! Flathub-style remote, both under `tests/fixtures/appstream/`. The icons
//! there are 1 px PNGs placed exactly where the tests need them to be: which
//! sizes exist is the point of half of these tests.
//!
//! `live_*` tests read the real catalogue through `Catalogue::load_system` and
//! are ignored by default; on a machine without the catalogue package, point
//! `BROKEY_APPSTREAM_DIR` at an extracted copy (a directory holding `xml/` and
//! `icons/`) and run `cargo test -p brokey-core -- --ignored live_`.

use brokey_core::Picture;
use brokey_core::appstream::Catalogue;
#[cfg(unix)]
use brokey_core::appstream::extra_roots;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/appstream")
}

fn swcatalog() -> PathBuf {
    fixtures().join("swcatalog")
}

fn icon(catalogue: &Catalogue, id: &str) -> Option<Picture> {
    catalogue
        .by_id(id)
        .unwrap_or_else(|| panic!("{id} is in the fixture"))
        .icon
        .clone()
}

fn ids<'a>(components: &[&'a brokey_core::appstream::Component]) -> Vec<&'a str> {
    components.iter().map(|c| c.id.as_str()).collect()
}

/// The distribution layout: every file under `xml/`, icons under the
/// sibling `icons/<origin>/`.
fn system() -> Catalogue {
    Catalogue::load_roots(&[swcatalog().join("xml")])
}

/// The Flatpak layout: `appstream/<remote>/<arch>/active/` under an
/// installation root, icons beside each file.
fn flatpak() -> Catalogue {
    Catalogue::load_flatpak_roots(&[fixtures().join("flatpak")])
}

#[test]
fn every_file_under_xml_is_read_in_name_order_with_its_own_origin() {
    let catalogue = system();
    // core.xml (plain), extra.xml (plain), multilib.xml.gz (gzip): 1 + 10 + 2.
    assert_eq!(catalogue.len(), 13);
    assert!(!catalogue.is_empty());
    assert_eq!(catalogue.components()[0].id, "links");
    assert_eq!(catalogue.components()[0].origin, "archlinux-arch-core");
    assert_eq!(
        catalogue.by_id("org.gimp.GIMP").unwrap().origin,
        "archlinux-arch-extra"
    );
    assert_eq!(
        catalogue.by_id("com.valvesoftware.Steam").unwrap().origin,
        "archlinux-arch-multilib"
    );
}

#[test]
fn gzip_and_plain_files_parse_through_the_same_entry_point() {
    let icons = swcatalog().join("icons");
    let gz = std::fs::read(swcatalog().join("xml/multilib.xml.gz")).unwrap();
    let zipped = Catalogue::parse(&gz, Some(&icons), None).unwrap();
    assert_eq!(zipped.len(), 2);
    assert_eq!(zipped[1].id, "com.valvesoftware.Steam");
    assert_eq!(
        zipped[1].icon,
        Some(Picture::File(
            icons.join("archlinux-arch-multilib/128x128/steam_steam.png")
        ))
    );
    let plain = std::fs::read(swcatalog().join("xml/core.xml")).unwrap();
    let parsed = Catalogue::parse(&plain, Some(&icons), None).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].name, "Links");
}

#[test]
fn translated_elements_are_ignored_wherever_they_come_in_the_file() {
    let catalogue = system();
    // Arabic and Bulgarian names come before the untranslated one.
    let gimp = catalogue.by_id("org.gimp.GIMP").unwrap();
    assert_eq!(gimp.name, "GNU Image Manipulation Program");
    assert_eq!(
        gimp.summary.as_deref(),
        Some("High-end image creation and manipulation")
    );
    assert_eq!(gimp.developer.as_deref(), Some("The GIMP team"));
    assert_eq!(gimp.keywords, vec!["GIMP", "Photoshop"]);
    // A Bulgarian description precedes the English one; a French one follows it.
    let description = gimp.description.as_deref().unwrap();
    assert!(
        description.starts_with("<p>GIMP is an acronym for GNU Image Manipulation Program."),
        "{description}"
    );
    assert!(!description.contains("GIMP е"), "{description}");
    assert!(!description.contains("acronyme"), "{description}");
    assert_eq!(
        gimp.screenshots[0].caption.as_deref(),
        Some(
            "Scene 4 cut 15 of \"ZeMarmot\" being edited in GIMP. Illustration by Aryeom (CC by-sa 4.0 International)"
        )
    );
    // Three translated names precede the English one here.
    let cli = catalogue.by_id("org.freedesktop.appstream.cli").unwrap();
    assert_eq!(cli.name, "AppStream CLI");
    assert_eq!(
        cli.summary.as_deref(),
        Some("An utility to work with AppStream metadata")
    );
    let zyn = catalogue.by_id("zynaddsubfx-alsa").unwrap();
    assert_eq!(
        zyn.summary.as_deref(),
        Some("A powerful realtime software synthesizer")
    );
}

#[test]
fn a_trailing_desktop_is_stripped_from_the_id() {
    let catalogue = system();
    for id in [
        "links",
        "gmic_qt",
        "zynaddsubfx-alsa",
        "zynaddsubfx-oss",
        "com.valvesoftware.Steam",
    ] {
        assert!(catalogue.by_id(id).is_some(), "{id}");
        assert!(
            catalogue.by_id(&format!("{id}.desktop")).is_none(),
            "{id}.desktop is not a key"
        );
    }
    // A `.desktop` in the middle of an id is left alone.
    assert_eq!(catalogue.by_id("gmic_qt").unwrap().name, "G'MIC-Qt");
}

#[test]
fn the_largest_icon_that_exists_on_disk_wins() {
    let catalogue = system();
    let icons = swcatalog().join("icons");
    // All three sizes declared and present: 128.
    assert_eq!(
        icon(&catalogue, "org.gimp.GIMP"),
        Some(Picture::File(
            icons.join("archlinux-arch-extra/128x128/gimp_gimp.png")
        ))
    );
    // Declared 48, 64 and 128; only 48 and 64 on disk.
    assert_eq!(
        icon(&catalogue, "zynaddsubfx-alsa"),
        Some(Picture::File(icons.join(
            "archlinux-arch-extra/64x64/zynaddsubfx_zynaddsubfx.png"
        )))
    );
    // Only 48 on disk.
    assert_eq!(
        icon(&catalogue, "org.gnome.Decibels"),
        Some(Picture::File(icons.join(
            "archlinux-arch-extra/48x48/decibels_org.gnome.Decibels.png"
        )))
    );
    // Each origin has its own directory under icons/.
    assert_eq!(
        icon(&catalogue, "com.valvesoftware.Steam"),
        Some(Picture::File(
            icons.join("archlinux-arch-multilib/128x128/steam_steam.png")
        ))
    );
    assert_eq!(
        icon(&catalogue, "io.github.xyproto.zsnes"),
        Some(Picture::File(icons.join(
            "archlinux-arch-multilib/48x48/zsnes_io.github.xyproto.zsnes.png"
        )))
    );
    // core declares one 64 px icon whose file name says 48.
    assert_eq!(
        icon(&catalogue, "links"),
        Some(Picture::File(
            icons.join("archlinux-arch-core/64x64/links_links-48x48.png")
        ))
    );
}

#[test]
fn a_missing_icon_file_gives_none_and_never_a_path() {
    let catalogue = system();
    // Declared in all three sizes, none on disk.
    assert_eq!(icon(&catalogue, "gmic_qt"), None);
    assert_eq!(icon(&catalogue, "org.cockpit_project.cockpit"), None);
    // No icon element at all.
    assert_eq!(icon(&catalogue, "com.github.whipper_team.Whipper"), None);
    assert_eq!(icon(&catalogue, "org.pwmt.zathura-djvu"), None);
    // A relative remote icon with no media_baseurl is not a URL the page
    // could fetch, and the cached files are absent.
    assert_eq!(icon(&catalogue, "de.urwpp.StandardSymbolsPS"), None);
    for component in catalogue.components() {
        if let Some(Picture::File(path)) = &component.icon {
            assert!(
                path.is_file(),
                "{}: {} does not exist",
                component.id,
                path.display()
            );
        }
    }
    // Without an icons directory nothing resolves, whatever is declared.
    let blind = Catalogue::load_dir(&swcatalog().join("xml"), None, None);
    assert_eq!(blind.len(), 13);
    assert!(blind.components().iter().all(|c| c.icon.is_none()));
}

#[test]
fn description_markup_round_trips_with_whitespace_collapsed() {
    let catalogue = system();
    // A paragraph indented over three lines becomes one line.
    assert_eq!(
        catalogue
            .by_id("com.github.whipper_team.Whipper")
            .unwrap()
            .description
            .as_deref(),
        Some(
            "<p>whipper is a command-line CD-DA ripper that focuses on making accurate rips over fast ones.</p>"
        )
    );
    // Lists keep their items; paragraphs are separated by a newline.
    assert_eq!(
        catalogue
            .by_id("org.gnome.Decibels")
            .unwrap()
            .description
            .as_deref(),
        Some(concat!(
            "<p>An audio player that just plays audio files. It doesn't require an organized music library and won't overload you with tons of functionality.</p>\n",
            "<p>Audio Player still offers advanced features such as:</p>\n",
            "<ul><li>An elegant waveform of the track</li><li>Adjustable playback speed</li><li>Easy seek controls</li><li>Playing multiple files at the same time</li></ul>"
        ))
    );
    // Inline markup survives.
    let cli = catalogue.by_id("org.freedesktop.appstream.cli").unwrap();
    assert!(
        cli.description
            .as_deref()
            .unwrap()
            .contains("The <em>appstreamcli</em> command-line tool allows")
    );
    // Release notes are not the description, even though they carry the same markup.
    let gimp = catalogue.by_id("org.gimp.GIMP").unwrap();
    assert!(
        !gimp
            .description
            .as_deref()
            .unwrap()
            .contains("bugfix release")
    );
}

#[test]
fn every_fact_the_detail_page_draws_is_read() {
    let catalogue = system();
    let gimp = catalogue.by_id("org.gimp.GIMP").unwrap();
    assert_eq!(gimp.pkgname.as_deref(), Some("gimp"));
    assert_eq!(gimp.bundle, None);
    assert_eq!(gimp.licence.as_deref(), Some("GPL-3.0+ AND LGPL-3.0+"));
    assert_eq!(gimp.homepage.as_deref(), Some("https://www.gimp.org/"));
    assert_eq!(
        gimp.categories,
        vec!["Graphics", "2DGraphics", "RasterGraphics"]
    );
    assert_eq!(
        gimp.latest_release,
        Some(("3.2.4".to_string(), Some(1_776_384_000)))
    );
    assert_eq!(gimp.screenshots.len(), 4);
    assert_eq!(
        gimp.screenshots[0].image,
        Picture::Url("https://www.gimp.org/screenshots/Screenshot-gimp-3.0-painting.jpg".into())
    );
    assert_eq!(gimp.screenshots[0].thumbnail, None);
    assert_eq!(
        (gimp.screenshots[0].width, gimp.screenshots[0].height),
        (None, None)
    );

    // Sizes come from the image attributes; the default screenshot goes first.
    let steam = catalogue.by_id("com.valvesoftware.Steam").unwrap();
    assert_eq!(steam.screenshots.len(), 3);
    assert_eq!(
        (steam.screenshots[0].width, steam.screenshots[0].height),
        (Some(1200), Some(582))
    );
    assert!(
        matches!(&steam.screenshots[0].image, Picture::Url(u) if u.ends_with("ac26dea63042eec4886d5fa27854517ce374b11e.jpg"))
    );
    assert_eq!(
        steam.latest_release,
        Some(("1.0.0.87".to_string(), Some(1_782_432_000)))
    );
    assert_eq!(steam.developer.as_deref(), Some("Valve Corporation"));
    assert_eq!(steam.licence.as_deref(), Some("LicenseRef-proprietary"));
    assert_eq!(
        steam.homepage.as_deref(),
        Some("https://store.steampowered.com/")
    );

    // A thumbnail beside a source image; the source has no declared size.
    let decibels = catalogue.by_id("org.gnome.Decibels").unwrap();
    assert_eq!(decibels.screenshots.len(), 2);
    assert_eq!(
        decibels.screenshots[0].image,
        Picture::Url(
            "https://static.gnome.org/appdata/gnome-48/decibels/playing-light-x2.png".into()
        )
    );
    assert_eq!(
        decibels.screenshots[0].thumbnail,
        Some(Picture::Url(
            "https://static.gnome.org/appdata/gnome-48/decibels/playing-light-x1.png".into()
        ))
    );
    assert_eq!(
        decibels.keywords,
        vec!["music", "player", "media", "audio", "decibels"]
    );
    assert_eq!(
        decibels.latest_release,
        Some(("49.6".to_string(), Some(1_775_865_600)))
    );

    // Only `<url type="homepage">` is the homepage.
    let djvu = catalogue.by_id("org.pwmt.zathura-djvu").unwrap();
    assert_eq!(
        djvu.homepage.as_deref(),
        Some("https://pwmt.org/projects/zathura-djvu/")
    );
    assert_eq!(djvu.developer.as_deref(), Some("pwmt"));
    assert_eq!(
        djvu.latest_release,
        Some(("2026.07.18".to_string(), Some(1_784_332_800)))
    );
}

#[test]
fn the_component_type_decides_what_is_an_application() {
    let catalogue = system();
    let expect = [
        ("org.gimp.GIMP", "desktop-application", true),
        (
            "com.github.whipper_team.Whipper",
            "console-application",
            true,
        ),
        ("org.freedesktop.appstream.cli", "console-application", true),
        ("org.cockpit_project.cockpit", "web-application", true),
        ("org.pwmt.zathura-djvu", "addon", false),
        ("de.urwpp.StandardSymbolsPS", "font", false),
    ];
    for (id, component_type, is_app) in expect {
        let c = catalogue.by_id(id).unwrap();
        assert_eq!(c.component_type, component_type, "{id}");
        assert_eq!(c.is_app, is_app, "{id}");
    }
}

#[test]
fn a_package_maps_to_its_desktop_application_first_and_keeps_the_rest() {
    let catalogue = system();
    // Two desktop applications from one package, neither named like it,
    // both legacy ids: the shorter name ("ZynAddSubFX - OSS") stands for
    // the package, whatever the catalogue's order.
    assert_eq!(
        catalogue.by_pkgname("zynaddsubfx").unwrap().id,
        "zynaddsubfx-oss"
    );
    assert_eq!(
        ids(&catalogue.by_pkgname_all("zynaddsubfx")),
        vec!["zynaddsubfx-alsa", "zynaddsubfx-oss"]
    );
    assert_eq!(catalogue.by_pkgname("gimp").unwrap().id, "org.gimp.GIMP");
    assert_eq!(
        catalogue.by_pkgname("steam").unwrap().id,
        "com.valvesoftware.Steam"
    );
    assert_eq!(catalogue.by_pkgname("links").unwrap().name, "Links");
    // A package whose only component is not an application still maps.
    assert_eq!(
        catalogue.by_pkgname("gsfonts").unwrap().id,
        "de.urwpp.StandardSymbolsPS"
    );
    assert_eq!(
        catalogue.by_pkgname("whipper").unwrap().component_type,
        "console-application"
    );
    assert!(catalogue.by_pkgname("nothing-provides-this").is_none());
    assert!(catalogue.by_pkgname_all("nothing-provides-this").is_empty());
    // Distribution catalogues have no bundles.
    assert!(catalogue.by_bundle("com.valvesoftware.Steam").is_none());
}

#[test]
fn search_is_case_insensitive_and_ranks_name_over_id_over_keyword_over_summary() {
    let catalogue = system();
    // Name exact.
    assert_eq!(
        ids(&catalogue.search("steam", 10)),
        vec!["com.valvesoftware.Steam"]
    );
    assert_eq!(
        ids(&catalogue.search("STEAM", 10)),
        vec!["com.valvesoftware.Steam"]
    );
    assert_eq!(
        ids(&catalogue.search("Audio Player", 10)),
        vec!["org.gnome.Decibels"]
    );
    // Name prefix beats a keyword match; equal keyword matches are ordered
    // by the shorter name, so "ZynAddSubFX - OSS" precedes "ZynAddSubFX - Alsa".
    assert_eq!(
        ids(&catalogue.search("audio", 10)),
        vec!["org.gnome.Decibels", "zynaddsubfx-oss", "zynaddsubfx-alsa"]
    );
    assert_eq!(
        ids(&catalogue.search("audio", 1)),
        vec!["org.gnome.Decibels"]
    );
    // The name does not contain "gimp"; the id does.
    assert_eq!(ids(&catalogue.search("Gimp", 10)), vec!["org.gimp.GIMP"]);
    // Name contains.
    assert_eq!(
        ids(&catalogue.search("player", 10)),
        vec!["org.gnome.Decibels"]
    );
    // Keyword equals, not contains: "synth" is a keyword, "synthes" is not
    // one and is not in a name or id, so it falls through to the summary.
    assert_eq!(
        ids(&catalogue.search("synth", 10)),
        vec!["zynaddsubfx-oss", "zynaddsubfx-alsa"]
    );
    assert_eq!(
        ids(&catalogue.search("synthes", 10)),
        vec!["zynaddsubfx-oss", "zynaddsubfx-alsa"]
    );
    // Summary contains, last resort.
    assert_eq!(ids(&catalogue.search("web browser", 10)), vec!["links"]);
    assert!(catalogue.search("nothing has this", 10).is_empty());
    assert!(catalogue.search("", 10).is_empty());
}

#[test]
fn the_flatpak_layout_reads_every_remote_with_its_name_as_origin() {
    let catalogue = flatpak();
    assert_eq!(catalogue.len(), 5);
    let steam = catalogue.by_id("com.valvesoftware.Steam").unwrap();
    assert_eq!(steam.origin, "flathub");
    assert_eq!(steam.pkgname, None);
    assert_eq!(
        steam.bundle.as_deref(),
        Some("app/com.valvesoftware.Steam/x86_64/stable")
    );
    assert_eq!(steam.name, "Steam");
    assert_eq!(
        steam.summary.as_deref(),
        Some("Manage and play games distributed by Steam")
    );
    assert_eq!(steam.developer.as_deref(), Some("Valve Corporation"));
    assert_eq!(steam.keywords, vec!["games", "valve"]);
    assert_eq!(
        steam.homepage.as_deref(),
        Some("https://store.steampowered.com")
    );
    assert_eq!(
        steam.latest_release,
        Some(("1.0.0.85".to_string(), Some(1_759_190_400)))
    );
    assert_eq!(
        steam.description.as_deref(),
        Some(concat!(
            "<p>Steam is a digital distribution platform for games and other software.</p>\n",
            "<p>This wrapper is not verified by, affiliated with, or supported by Valve Corporation.</p>\n",
            "<p>Note: this package uses <code>flatpak run com.valvesoftware.Steam</code> and <em>needs</em> a 64-bit system.</p>"
        ))
    );
    // The largest thumbnail rides beside the source image.
    assert_eq!(steam.screenshots.len(), 1);
    let shot = &steam.screenshots[0];
    assert_eq!(shot.caption.as_deref(), Some("The Steam store"));
    assert_eq!((shot.width, shot.height), (Some(1920), Some(1080)));
    assert!(matches!(&shot.image, Picture::Url(u) if u.ends_with("image-1_orig.png")));
    assert!(matches!(&shot.thumbnail, Some(Picture::Url(u)) if u.ends_with("image-1_752x423.png")));

    let devel = catalogue.by_id("org.gnome.Decibels.Devel").unwrap();
    assert_eq!(devel.origin, "gnome-nightly");
    assert_eq!(
        catalogue
            .by_id("org.freedesktop.Platform")
            .unwrap()
            .component_type,
        "runtime"
    );
    assert!(!catalogue.by_id("org.freedesktop.Platform").unwrap().is_app);
    assert!(
        !catalogue
            .by_id("com.valvesoftware.Steam.CompatibilityTool.Proton")
            .unwrap()
            .is_app
    );
}

#[test]
fn flatpak_icons_live_in_a_flat_directory_beside_each_remote() {
    let catalogue = flatpak();
    let flathub = fixtures().join("flatpak/appstream/flathub/x86_64/active/icons");
    // 64 and 128 declared and present: 128.
    assert_eq!(
        icon(&catalogue, "com.valvesoftware.Steam"),
        Some(Picture::File(
            flathub.join("128x128/com.valvesoftware.Steam.png")
        ))
    );
    // Cached files missing: the remote URL, which Flathub always gives.
    assert_eq!(
        icon(&catalogue, "org.gimp.GIMP"),
        Some(Picture::Url("https://dl.flathub.org/media/org/gimp/GIMP/e6b0c0d5b8a1/icons/128x128/org.gimp.GIMP.png".into()))
    );
    assert_eq!(
        icon(&catalogue, "org.freedesktop.Platform"),
        Some(Picture::File(
            flathub.join("64x64/org.freedesktop.Platform.png")
        ))
    );
    assert_eq!(
        icon(
            &catalogue,
            "com.valvesoftware.Steam.CompatibilityTool.Proton"
        ),
        None
    );
    // The other remote resolves against its own directory.
    assert_eq!(
        icon(&catalogue, "org.gnome.Decibels.Devel"),
        Some(Picture::File(
            fixtures().join("flatpak/appstream/gnome-nightly/x86_64/active/icons/128x128/org.gnome.Decibels.Devel.png")
        ))
    );
}

#[test]
fn bundles_are_found_by_full_ref_and_by_id_segment() {
    let catalogue = flatpak();
    assert_eq!(
        catalogue
            .by_bundle("app/com.valvesoftware.Steam/x86_64/stable")
            .unwrap()
            .name,
        "Steam"
    );
    assert_eq!(
        catalogue.by_bundle("com.valvesoftware.Steam").unwrap().name,
        "Steam"
    );
    assert_eq!(
        catalogue
            .by_bundle("runtime/org.freedesktop.Platform/x86_64/24.08")
            .unwrap()
            .name,
        "Freedesktop Platform"
    );
    assert_eq!(
        catalogue
            .by_bundle("org.freedesktop.Platform")
            .unwrap()
            .name,
        "Freedesktop Platform"
    );
    assert_eq!(
        catalogue
            .by_bundle("app/org.gnome.Decibels.Devel/x86_64/master")
            .unwrap()
            .origin,
        "gnome-nightly"
    );
    assert!(
        catalogue
            .by_bundle("app/org.gimp.GIMP/aarch64/stable")
            .is_none()
    );
    assert!(
        catalogue.by_pkgname("steam").is_none(),
        "a remote's catalogue has no pkgname"
    );
    // Both catalogues key the same application by the same id.
    assert_eq!(
        system().by_id("com.valvesoftware.Steam").unwrap().id,
        catalogue.by_id("com.valvesoftware.Steam").unwrap().id
    );
}

#[test]
fn search_in_a_remote_puts_applications_before_add_ons_on_a_tie() {
    let catalogue = flatpak();
    // Both ids contain "valve"; the application wins the tie.
    assert_eq!(
        ids(&catalogue.search("valve", 10)),
        vec![
            "com.valvesoftware.Steam",
            "com.valvesoftware.Steam.CompatibilityTool.Proton"
        ]
    );
    assert_eq!(
        ids(&catalogue.search("audio", 10)),
        vec!["org.gnome.Decibels.Devel"]
    );
}

#[test]
fn the_callers_origin_overrides_the_files() {
    let catalogue = Catalogue::load_dir(
        &swcatalog().join("xml"),
        Some(&swcatalog().join("icons")),
        Some("custom"),
    );
    assert_eq!(catalogue.len(), 13);
    assert!(catalogue.components().iter().all(|c| c.origin == "custom"));
    // With the origin renamed, icons/<origin>/ does not exist and the
    // flat layout is tried instead, where nothing is: no paths are invented.
    assert!(catalogue.components().iter().all(|c| c.icon.is_none()));
}

// `extra_roots` splits on `:`, which is also how a Windows path names its
// drive (`C:\...`), so a real path is not representable in the variable
// there yet. Linux only, like `BROKEY_APPSTREAM_DIR` itself for now.
#[cfg(unix)]
#[test]
fn brokey_appstream_dir_names_roots_that_load_like_the_system_ones() {
    // What `load_system` does with the variable, without touching the
    // process environment: split, keep absolute entries, read each `xml/`.
    let value = format!(":{}:relative/path:", swcatalog().display());
    let roots = extra_roots(Some(&value));
    assert_eq!(roots, vec![swcatalog()]);
    let xml_dirs: Vec<PathBuf> = roots.iter().map(|r| r.join("xml")).collect();
    let catalogue = Catalogue::load_roots(&xml_dirs);
    assert_eq!(catalogue.len(), 13);
    assert!(matches!(icon(&catalogue, "org.gimp.GIMP"), Some(Picture::File(p)) if p.is_file()));
}

#[test]
fn missing_directories_are_simply_absent() {
    let catalogue = Catalogue::load_roots(&[
        PathBuf::from("/nonexistent/swcatalog/xml"),
        swcatalog().join("xml"),
    ]);
    assert_eq!(catalogue.len(), 13);
    assert!(Catalogue::load_flatpak_roots(&[PathBuf::from("/nonexistent/flatpak")]).is_empty());
    assert!(Catalogue::load_dir(Path::new("/nonexistent"), None, None).is_empty());
}

/// The real thing. Needs the distribution's catalogue package, or
/// `BROKEY_APPSTREAM_DIR` pointing at an extracted copy of it.
#[test]
#[ignore]
fn live_the_distribution_catalogue_loads_with_icons_on_disk() {
    let system = brokey_core::system::detect();
    let started = Instant::now();
    let catalogue = Catalogue::load_system(&system);
    let first = started.elapsed();
    let started = Instant::now();
    let again = Catalogue::load_system(&system);
    let second = started.elapsed();
    println!(
        "{} components; first load {first:?}, second load (memoised) {second:?}; BROKEY_APPSTREAM_DIR={:?}",
        catalogue.len(),
        std::env::var("BROKEY_APPSTREAM_DIR").ok()
    );
    assert!(
        catalogue.len() > 1000,
        "only {} components: install the AppStream catalogue package or set BROKEY_APPSTREAM_DIR to an extracted copy",
        catalogue.len()
    );
    assert_eq!(again.len(), catalogue.len());
    let steam = catalogue
        .by_pkgname("steam")
        .expect("the steam package has a component");
    assert_eq!(steam.id, "com.valvesoftware.Steam");
    match &steam.icon {
        Some(Picture::File(path)) => assert!(path.is_file(), "{} does not exist", path.display()),
        other => panic!("steam's icon should be a file on disk, got {other:?}"),
    }
    println!("steam: {:?}", steam.icon);
    let apps = catalogue.components().iter().filter(|c| c.is_app).count();
    let with_icon = catalogue
        .components()
        .iter()
        .filter(|c| c.icon.is_some())
        .count();
    println!("{apps} applications, {with_icon} with an icon");
    for component in catalogue.components() {
        if let Some(Picture::File(path)) = &component.icon {
            assert!(
                path.is_file(),
                "{}: {} does not exist",
                component.id,
                path.display()
            );
        }
    }
    let hits = catalogue.search("steam", 5);
    println!("search steam: {:?}", ids(&hits));
    assert_eq!(hits[0].id, "com.valvesoftware.Steam");
}
