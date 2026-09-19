//! The dnf source against recorded command output in both generations'
//! shapes, through a runner that answers from fixtures and records what it
//! was asked, and rpm's version ordering against rpm's own test table.
//! Nothing here runs dnf or rpm; on a machine that is not Fedora-based the
//! source must say so, and that is tested too.

#![cfg(unix)]

use brokey_core::appstream::Catalogue;
use brokey_core::sources::linux::dnf::{
    Dnf, Output, QUERY_FORMAT, RPM_FORMAT, Runner, parse_check_update, parse_rows, parse_time,
    rpm_compare, rpmvercmp, search_glob,
};
use brokey_core::{
    Error, Op, PackageKind, PackageRef, Platform, Query, Result, Source, SourceKind, SystemInfo,
};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dnf")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn fedora() -> SystemInfo {
    SystemInfo {
        distro_id: "fedora".to_string(),
        distro_like: Vec::new(),
        pretty_name: "Fedora Linux 41 (Workstation Edition)".to_string(),
        arch: "x86_64".to_string(),
        desktop: None,
        session: None,
        platform: Platform::Linux,
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
        platform: Platform::Linux,
    }
}

fn catalogue() -> Arc<Catalogue> {
    Arc::new(Catalogue::default())
}

fn dnf_ref(name: &str) -> PackageRef {
    PackageRef {
        source: SourceKind::Dnf,
        id: name.to_string(),
    }
}

/// Which machine the fixtures stand in for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Machine {
    /// dnf5: seconds for build times, one row per line, headerless check-upgrade.
    Fedora41,
    /// dnf4: dated build times, a blank line between rows, "Obsoleting Packages".
    Fedora39,
    /// dnf5 whose repositories will not load: every dnf query fails, rpm works.
    Offline,
    /// dnf5 with nothing to update: check-update exits 0.
    UpToDate,
}

type Calls = Arc<Mutex<Vec<(String, Vec<String>)>>>;

struct Fixtures {
    machine: Machine,
    calls: Calls,
}

impl Runner for Fixtures {
    fn run(&self, program: &str, args: &[String]) -> Result<Output> {
        self.calls
            .lock()
            .unwrap()
            .push((program.to_string(), args.to_vec()));
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        let failed = || {
            Ok(Output {
                code: Some(1),
                stdout: String::new(),
                stderr: "Error: Failed to download metadata for repo 'updates': Cannot prepare internal mirrorlist\n"
                    .to_string(),
            })
        };
        match (program, a.as_slice()) {
            ("dnf", ["--version"]) => Ok(Output::new(
                0,
                fixture(if self.machine == Machine::Fedora39 {
                    "version-dnf4.txt"
                } else {
                    "version-dnf5.txt"
                }),
            )),
            ("dnf", _) if self.machine == Machine::Offline => failed(),
            (
                "dnf",
                [
                    "repoquery",
                    "-q",
                    "--installed",
                    "--queryformat",
                    format,
                    rest @ ..,
                ],
            ) if *format == QUERY_FORMAT => Ok(Output::new(
                0,
                match rest {
                    [] => fixture("repoquery-installed.txt"),
                    [key] if key.contains("gimp") => fixture("repoquery-gimp-installed.txt"),
                    _ => String::new(),
                },
            )),
            ("dnf", ["repoquery", "-q", "--queryformat", format, key])
                if *format == QUERY_FORMAT =>
            {
                Ok(Output::new(
                    0,
                    if key.contains("gimp") {
                        fixture(if self.machine == Machine::Fedora39 {
                            "repoquery-gimp-dnf4.txt"
                        } else {
                            "repoquery-gimp.txt"
                        })
                    } else {
                        String::new()
                    },
                ))
            }
            ("dnf", ["repoquery", "-q", "--queryformat", format, "gimp"])
                if format.starts_with("%{description}") =>
            {
                Ok(Output::new(0, fixture("description-gimp.txt")))
            }
            ("dnf", ["check-update", "-q"]) => Ok(match self.machine {
                Machine::UpToDate => Output::new(0, ""),
                Machine::Fedora39 => Output::new(100, fixture("check-update-dnf4.txt")),
                Machine::Fedora41 | Machine::Offline => {
                    Output::new(100, fixture("check-upgrade-dnf5.txt"))
                }
            }),
            ("rpm", ["-qa", "--queryformat", format, ..]) if *format == RPM_FORMAT => {
                Ok(Output::new(0, fixture("rpm-qa.txt")))
            }
            _ => Err(Error::new(format!(
                "the fixtures have no answer for {program} {}",
                args.join(" ")
            ))),
        }
    }
}

/// The source as that machine would build it, and the calls it makes.
fn source(machine: Machine) -> (Dnf, Calls) {
    let calls: Calls = Arc::default();
    let runner = Fixtures {
        machine,
        calls: calls.clone(),
    };
    let dnf = Dnf::with_runner(
        &fedora(),
        catalogue(),
        Some(PathBuf::from("/usr/bin/dnf")),
        Box::new(runner),
    );
    (dnf, calls)
}

fn args(calls: &Calls) -> Vec<Vec<String>> {
    calls
        .lock()
        .unwrap()
        .iter()
        .map(|(program, args)| {
            std::iter::once(program.clone())
                .chain(args.iter().cloned())
                .collect()
        })
        .collect()
}

fn dnf_args(list: &[&str]) -> Vec<String> {
    std::iter::once("dnf")
        .chain(list.iter().copied())
        .map(str::to_string)
        .collect()
}

fn sign(o: Ordering) -> i8 {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

#[test]
fn rpmvercmp_agrees_with_rpm_s_own_test_suite() {
    // Every row is from rpm's tests/rpmvercmp.at, which runs the real
    // rpmvercmp; the answers are rpm's, not this test's.
    const TABLE: &[(&str, &str, i8)] = &[
        ("1.0", "1.0", 0),
        ("1.0", "2.0", -1),
        ("2.0.1", "2.0.1", 0),
        ("2.0", "2.0.1", -1),
        ("2.0.1a", "2.0.1a", 0),
        ("2.0.1a", "2.0.1", 1),
        ("5.5p1", "5.5p1", 0),
        ("5.5p1", "5.5p2", -1),
        ("5.5p10", "5.5p10", 0),
        ("5.5p1", "5.5p10", -1),
        ("10xyz", "10.1xyz", -1),
        ("xyz10", "xyz10", 0),
        ("xyz10", "xyz10.1", -1),
        ("xyz.4", "xyz.4", 0),
        ("xyz.4", "8", -1),
        ("xyz.4", "2", -1),
        ("5.5p2", "5.6p1", -1),
        ("5.6p1", "6.5p1", -1),
        ("6.0.rc1", "6.0", 1),
        ("10b2", "10a1", 1),
        ("10a2", "10b2", -1),
        ("1.0aa", "1.0aa", 0),
        ("1.0a", "1.0aa", -1),
        ("10.0001", "10.0001", 0),
        ("10.0001", "10.1", 0),
        ("10.0001", "10.0039", -1),
        ("4.999.9", "5.0", -1),
        ("20101121", "20101121", 0),
        ("20101121", "20101122", -1),
        ("2_0", "2_0", 0),
        ("2.0", "2_0", 0),
        ("a", "a", 0),
        ("a+", "a+", 0),
        ("a+", "a_", 0),
        ("+a", "+a", 0),
        ("+a", "_a", 0),
        ("+_", "+_", 0),
        ("_+", "+_", 0),
        ("+", "_", 0),
        // Tilde.
        ("1.0~rc1", "1.0~rc1", 0),
        ("1.0~rc1", "1.0", -1),
        ("1.0~rc1", "1.0~rc2", -1),
        ("1.0~rc1~git123", "1.0~rc1~git123", 0),
        ("1.0~rc1~git123", "1.0~rc1", -1),
        // Caret.
        ("1.0^", "1.0^", 0),
        ("1.0^", "1.0", 1),
        ("1.0^git1", "1.0^git1", 0),
        ("1.0^git1", "1.0", 1),
        ("1.0^git1", "1.0^git2", -1),
        ("1.0^git1", "1.01", -1),
        ("1.0^20160101", "1.0^20160101", 0),
        ("1.0^20160101", "1.0.1", -1),
        ("1.0^20160101^git1", "1.0^20160101^git1", 0),
        ("1.0^20160102", "1.0^20160101^git1", 1),
        // Tilde and caret together.
        ("1.0~rc1^git1", "1.0~rc1^git1", 0),
        ("1.0~rc1^git1", "1.0~rc1", 1),
        ("1.0^git1~pre", "1.0^git1~pre", 0),
        ("1.0^git1", "1.0^git1~pre", 1),
    ];
    assert!(TABLE.len() >= 30);
    for (a, b, want) in TABLE {
        assert_eq!(sign(rpmvercmp(a, b)), *want, "rpmvercmp({a}, {b})");
        assert_eq!(sign(rpmvercmp(b, a)), -*want, "rpmvercmp({b}, {a})");
    }
}

#[test]
fn rpm_compare_orders_epoch_then_version_then_release() {
    const TABLE: &[(&str, &str, i8)] = &[
        ("1.0-1.fc40", "1.0-1.fc40", 0),
        ("1.0-2.fc40", "1.0-1.fc40", 1),
        ("1.0-1.fc41", "1.0-1.fc40", 1),
        ("1.1-1.fc40", "1.0-9.fc40", 1),
        ("1:1.0-1.fc40", "2.0-1.fc40", 1),
        ("0:1.0-1.fc40", "1.0-1.fc40", 0),
        ("2:9.1.496-1.fc40", "2:9.1.452-1.fc40", 1),
        ("1.0-1", "1.0", 1),
        ("1.0~rc1-1.fc40", "1.0-1.fc40", -1),
        ("1.0^git1-1.fc40", "1.0-1.fc40", 1),
        ("6.9.7-200.fc40", "6.9.6-200.fc40", 1),
        ("6.10.0-0.rc1.20240601gitabc.1.fc41", "6.9.9-200.fc40", 1),
    ];
    for (a, b, want) in TABLE {
        assert_eq!(sign(rpm_compare(a, b)), *want, "rpm_compare({a}, {b})");
        assert_eq!(sign(rpm_compare(b, a)), -*want, "rpm_compare({b}, {a})");
    }
}

#[test]
fn on_this_machine_dnf_says_why_it_is_unavailable() {
    let system = brokey_core::system::detect();
    let dnf = Dnf::new(&system, catalogue());
    let status = dnf.status();
    assert_eq!(status.kind, SourceKind::Dnf);
    if system.is_fedora_like() {
        // The development machine is Arch; on a Fedora machine this test
        // has nothing to say and the fixture tests carry the weight.
        return;
    }
    assert!(!status.available);
    assert_eq!(
        status.reason,
        Some(format!(
            "dnf is for Fedora-based systems; this is {}.",
            system.pretty_name
        ))
    );
    assert_eq!(status.detail, None);
    let err = dnf.search(&Query::new("gimp")).unwrap_err();
    assert_eq!(err.source_kind, Some(SourceKind::Dnf));
    assert_eq!(Some(err.message), status.reason);
}

#[test]
fn a_cachyos_machine_is_told_dnf_is_for_fedora() {
    let calls: Calls = Arc::default();
    let runner = Fixtures {
        machine: Machine::Fedora41,
        calls: calls.clone(),
    };
    let dnf = Dnf::with_runner(
        &cachyos(),
        catalogue(),
        Some(PathBuf::from("/usr/bin/dnf")),
        Box::new(runner),
    );
    let status = dnf.status();
    assert!(!status.available);
    assert_eq!(
        status.reason.as_deref(),
        Some("dnf is for Fedora-based systems; this is CachyOS.")
    );
    let reason = "dnf is for Fedora-based systems; this is CachyOS.";
    assert_eq!(dnf.search(&Query::new("gimp")).unwrap_err().message, reason);
    assert_eq!(dnf.installed().unwrap_err().message, reason);
    assert_eq!(dnf.updates().unwrap_err().message, reason);
    assert_eq!(dnf.details("gimp").unwrap_err().message, reason);
    assert_eq!(
        dnf.plan(&Op::Refresh {
            source: SourceKind::Dnf
        })
        .unwrap_err()
        .message,
        reason
    );
    assert!(
        args(&calls).is_empty(),
        "an unavailable source never runs anything"
    );
}

#[test]
fn a_fedora_machine_without_dnf_says_so() {
    let runner = Fixtures {
        machine: Machine::Fedora41,
        calls: Arc::default(),
    };
    let dnf = Dnf::with_runner(&fedora(), catalogue(), None, Box::new(runner));
    assert_eq!(
        dnf.status().reason.as_deref(),
        Some("dnf is not installed, so packages cannot be changed.")
    );
}

#[test]
fn a_fedora_machine_reports_the_dnf_version_once() {
    let (dnf, calls) = source(Machine::Fedora41);
    let status = dnf.status();
    assert!(status.available);
    assert_eq!(status.reason, None);
    assert_eq!(status.detail.as_deref(), Some("dnf 5.2.6.2"));
    assert_eq!(dnf.status().detail.as_deref(), Some("dnf 5.2.6.2"));
    assert_eq!(
        args(&calls),
        vec![dnf_args(&["--version"])],
        "asked once, not per status call"
    );

    let (old, _) = source(Machine::Fedora39);
    assert_eq!(old.status().detail.as_deref(), Some("dnf 4.18.2"));
}

#[test]
fn search_asks_for_available_and_installed_rows_with_a_glob() {
    let (dnf, calls) = source(Machine::Fedora41);
    let found = dnf.search(&Query::new("gimp")).unwrap();
    let names: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        names,
        [
            "gimp",
            "gimp-data-extras",
            "gimp-devel",
            "gimp-libs",
            "gmic-gimp"
        ]
    );
    assert_eq!(
        args(&calls),
        vec![
            dnf_args(&["repoquery", "-q", "--queryformat", QUERY_FORMAT, "*gimp*"]),
            dnf_args(&[
                "repoquery",
                "-q",
                "--installed",
                "--queryformat",
                QUERY_FORMAT,
                "*gimp*"
            ]),
        ]
    );

    let gimp = &found[0];
    assert_eq!(gimp.source, SourceKind::Dnf);
    assert_eq!(gimp.name, "gimp");
    assert_eq!(gimp.kind, PackageKind::Package);
    assert_eq!(
        gimp.version.as_deref(),
        Some("2.10.38-1.fc40"),
        "the newest of the two available rows"
    );
    assert_eq!(gimp.repo.as_deref(), Some("updates"));
    assert!(gimp.installed);
    assert_eq!(gimp.installed_version.as_deref(), Some("2.10.36-4.fc40"));
    assert_eq!(
        gimp.summary.as_deref(),
        Some("GNU Image Manipulation Program")
    );
    assert_eq!(gimp.homepage.as_deref(), Some("https://www.gimp.org/"));
    assert_eq!(
        gimp.licence.as_deref(),
        Some("GPL-3.0-or-later AND LGPL-3.0-or-later")
    );
    assert_eq!(gimp.download_size, Some(23_100_200));
    assert_eq!(gimp.installed_size, Some(123_999_999));
    assert_eq!(gimp.updated, Some(1_718_000_000));
    assert_eq!(gimp.description, None);
    assert!(gimp.facts.is_empty());

    let extras = &found[1];
    assert_eq!(extras.homepage, None, "an empty url column is no homepage");
    assert!(!extras.installed);
    assert_eq!(extras.installed_version, None);
    assert!(found[3].installed, "gimp-libs is installed");
    assert!(!found[4].installed);
}

#[test]
fn search_for_nothing_asks_nothing() {
    let (dnf, calls) = source(Machine::Fedora41);
    assert!(dnf.search(&Query::new("  ")).unwrap().is_empty());
    assert!(args(&calls).is_empty());
    assert!(dnf.search(&Query::new("nonesuch")).unwrap().is_empty());
}

#[test]
fn search_honours_the_limit() {
    let (dnf, _) = source(Machine::Fedora41);
    let mut query = Query::new("gimp");
    query.limit = 2;
    let found = dnf.search(&query).unwrap();
    let names: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(names, ["gimp", "gimp-data-extras"]);
}

#[test]
fn search_reads_dnf4_s_dated_rows_too() {
    let (dnf, _) = source(Machine::Fedora39);
    let found = dnf.search(&Query::new("gimp")).unwrap();
    let names: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(names, ["gimp", "gimp-data-extras", "gimp-libs"]);
    assert_eq!(
        found[0].updated,
        Some(1_712_003_160),
        "2024-04-01 20:26 UTC"
    );
    assert_eq!(found[0].licence.as_deref(), Some("GPLv3+ and LGPLv3+"));
    assert_eq!(found[1].homepage, None, "(none) is no homepage");
    assert!(found[2].installed, "an @System row counts as installed");
    assert_eq!(
        found[2].repo, None,
        "@System is not a repository the page names"
    );
}

#[test]
fn installed_lists_the_newest_of_each_name() {
    let (dnf, calls) = source(Machine::Fedora41);
    let installed = dnf.installed().unwrap();
    let names: Vec<&str> = installed.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        names,
        [
            "a-very-long-package-name-that-overflows-the-column",
            "bash",
            "brokey",
            "gimp",
            "gimp-libs",
            "kernel",
            "vim-enhanced"
        ]
    );
    assert_eq!(
        args(&calls),
        vec![dnf_args(&[
            "repoquery",
            "-q",
            "--installed",
            "--queryformat",
            QUERY_FORMAT
        ])]
    );
    let kernel = installed.iter().find(|p| p.id == "kernel").unwrap();
    assert_eq!(
        kernel.installed_version.as_deref(),
        Some("6.9.6-200.fc40"),
        "two kernels, the newest named"
    );
    assert_eq!(kernel.installed_size, None, "a zero size is unknown");
    assert!(
        installed
            .iter()
            .all(|p| p.installed && p.version.is_none() && p.repo.is_none())
    );
}

#[test]
fn installed_falls_back_to_rpm_when_dnf_cannot_load_its_repositories() {
    let (dnf, calls) = source(Machine::Offline);
    let installed = dnf.installed().unwrap();
    let names: Vec<&str> = installed.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        names,
        ["bash", "gimp", "kernel"],
        "gpg-pubkey is a key, not a package"
    );
    assert_eq!(
        args(&calls),
        vec![
            dnf_args(&[
                "repoquery",
                "-q",
                "--installed",
                "--queryformat",
                QUERY_FORMAT
            ]),
            ["rpm", "-qa", "--queryformat", RPM_FORMAT]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        ]
    );
    let gimp = &installed[1];
    assert_eq!(gimp.installed_version.as_deref(), Some("2.10.36-4.fc40"));
    assert_eq!(gimp.installed_size, Some(123_456_789));
    assert_eq!(gimp.download_size, None);
    assert_eq!(gimp.updated, Some(1_712_000_000));

    // A search cannot fall back: there is no rpm for what is not installed.
    let err = dnf.search(&Query::new("gimp")).unwrap_err();
    assert_eq!(
        err.message,
        "dnf repoquery failed: Error: Failed to download metadata for repo 'updates': Cannot prepare internal mirrorlist. \
         Check the repository configuration and the network."
    );
    let err = dnf.updates().unwrap_err();
    assert!(
        err.message
            .starts_with("dnf check-update failed: Error: Failed to download metadata"),
        "{}",
        err.message
    );
}

#[test]
fn updates_come_from_check_update_and_the_installed_rows() {
    let (dnf, calls) = source(Machine::Fedora39);
    let updates = dnf.updates().unwrap();
    let rows: Vec<(&str, Option<&str>, &str)> = updates
        .iter()
        .map(|u| (u.name.as_str(), u.from.as_deref(), u.to.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            (
                "a-very-long-package-name-that-overflows-the-column",
                Some("1.2.2-1.fc40"),
                "1.2.3-1.fc40"
            ),
            ("brokey", Some("0.1.0-1.fc40"), "0.2.0-1.fc40"),
            ("gimp", Some("2.10.36-4.fc40"), "2.10.38-1.fc40"),
            ("gimp-libs", Some("2.10.36-4.fc40"), "2.10.38-1.fc40"),
            ("kernel", Some("6.9.6-200.fc40"), "6.9.7-200.fc40"),
            ("vim-enhanced", Some("9.1.452-1.fc40"), "2:9.1.496-1.fc40"),
        ],
        "the obsoleting section is not updates"
    );
    assert_eq!(
        args(&calls),
        vec![
            dnf_args(&["check-update", "-q"]),
            dnf_args(&[
                "repoquery",
                "-q",
                "--installed",
                "--queryformat",
                QUERY_FORMAT
            ]),
        ]
    );
    assert!(updates[1].is_self);
    assert!(!updates[2].is_self);
    assert_eq!(updates[2].package, dnf_ref("gimp"));
    assert_eq!(
        updates[2].summary.as_deref(),
        Some("GNU Image Manipulation Program")
    );
    assert_eq!(updates[2].kind, PackageKind::Package);
    assert_eq!(updates[2].download_size, None, "check-update does not say");
}

#[test]
fn updates_from_dnf5_skip_obsoleted_rows() {
    let (dnf, _) = source(Machine::Fedora41);
    let updates = dnf.updates().unwrap();
    let rows: Vec<(&str, Option<&str>, &str)> = updates
        .iter()
        .map(|u| (u.name.as_str(), u.from.as_deref(), u.to.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("brokey", Some("0.1.0-1.fc40"), "0.2.0-1.fc41"),
            ("gimp", Some("2.10.36-4.fc40"), "2.10.38-1.fc41"),
            ("kernel", Some("6.9.6-200.fc40"), "6.11.4-300.fc41"),
            ("new-thing", None, "1.0-1.fc41"),
            ("vim-enhanced", Some("9.1.452-1.fc40"), "2:9.1.700-1.fc41"),
        ],
        "old-thing is indented under new-thing and is what goes away"
    );
}

#[test]
fn no_updates_is_exit_zero_and_asks_nothing_else() {
    let (dnf, calls) = source(Machine::UpToDate);
    assert!(dnf.updates().unwrap().is_empty());
    assert_eq!(args(&calls), vec![dnf_args(&["check-update", "-q"])]);
}

#[test]
fn details_add_facts_and_the_description() {
    let (dnf, calls) = source(Machine::Fedora41);
    let gimp = dnf.details("gimp").unwrap();
    assert_eq!(gimp.id, "gimp");
    assert_eq!(gimp.version.as_deref(), Some("2.10.38-1.fc40"));
    assert!(gimp.installed);
    assert_eq!(
        gimp.facts,
        vec![
            ("Repository".to_string(), "updates".to_string()),
            (
                "Licence".to_string(),
                "GPL-3.0-or-later AND LGPL-3.0-or-later".to_string()
            ),
        ]
    );
    assert_eq!(
        gimp.description.as_deref(),
        Some(
            "<p>GIMP is a free and open source image editor. It can be used to retouch photos, compose images and \
             create original artwork.</p><p>It has a large collection of professional-level editing tools and filters.</p>"
        )
    );
    let calls = args(&calls);
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[0],
        dnf_args(&["repoquery", "-q", "--queryformat", QUERY_FORMAT, "gimp"])
    );
    assert_eq!(
        calls[1],
        dnf_args(&[
            "repoquery",
            "-q",
            "--installed",
            "--queryformat",
            QUERY_FORMAT,
            "gimp"
        ])
    );
    assert_eq!(
        calls[2][..4],
        dnf_args(&["repoquery", "-q", "--queryformat"])
    );
    assert!(
        calls[2][4].starts_with("%{description}"),
        "a separate query for the multi-line field"
    );
    assert_eq!(calls[2][5], "gimp");
}

#[test]
fn details_of_an_unknown_name_say_so() {
    let (dnf, _) = source(Machine::Fedora41);
    let err = dnf.details("nonesuch").unwrap_err();
    assert_eq!(
        err.message,
        "nonesuch is not in any enabled repository and is not installed. Refresh and search again."
    );
}

#[test]
fn plans_are_dnf_as_root_with_a_c_locale() {
    let (dnf, calls) = source(Machine::Fedora41);
    let install = dnf
        .plan(&Op::Install {
            package: dnf_ref("gimp"),
        })
        .unwrap();
    assert_eq!(install.len(), 1);
    let step = &install[0];
    assert_eq!(step.source, SourceKind::Dnf);
    assert_eq!(step.title, "Installing gimp");
    assert_eq!(step.command.program, "dnf");
    assert_eq!(step.command.args, ["install", "-y", "gimp"]);
    assert_eq!(
        step.command.env,
        [("LC_ALL".to_string(), "C.UTF-8".to_string())]
    );
    assert_eq!(step.command.cwd, None);

    let remove = dnf
        .plan(&Op::Remove {
            package: dnf_ref("gimp"),
        })
        .unwrap();
    assert_eq!(remove[0].command.args, ["remove", "-y", "gimp"]);
    assert_eq!(remove[0].title, "Removing gimp");

    let update = dnf
        .plan(&Op::Update {
            package: dnf_ref("gimp"),
        })
        .unwrap();
    assert_eq!(update[0].command.args, ["upgrade", "-y", "gimp"]);
    assert_eq!(update[0].title, "Updating gimp");

    let all = dnf
        .plan(&Op::UpdateAll {
            source: SourceKind::Dnf,
        })
        .unwrap();
    assert_eq!(all[0].command.args, ["upgrade", "-y"]);
    assert_eq!(all[0].title, "Updating all dnf packages");
    assert!(all[0].weight > update[0].weight);

    let refresh = dnf
        .plan(&Op::Refresh {
            source: SourceKind::Dnf,
        })
        .unwrap();
    assert_eq!(refresh[0].command.args, ["makecache"]);
    assert_eq!(refresh[0].title, "Refreshing the dnf metadata");
    assert!(refresh[0].weight < update[0].weight);

    for step in install
        .iter()
        .chain(&remove)
        .chain(&update)
        .chain(&all)
        .chain(&refresh)
    {
        assert!(step.needs_root, "{}", step.title);
        assert_eq!(step.command.program, "dnf");
        assert!(
            !step.title.contains('\u{2014}'),
            "no em dashes: {}",
            step.title
        );
    }
    assert!(
        args(&calls).is_empty(),
        "a plan describes; it never runs dnf"
    );
}

#[test]
fn plans_refuse_a_package_from_another_source() {
    let (dnf, _) = source(Machine::Fedora41);
    let snap = PackageRef {
        source: SourceKind::Snap,
        id: "gimp".to_string(),
    };
    let err = dnf.plan(&Op::Install { package: snap }).unwrap_err();
    assert_eq!(err.message, "gimp is a Snap package, not a dnf package.");
    assert_eq!(err.source_kind, Some(SourceKind::Dnf));
}

#[test]
fn check_update_output_parses_in_both_generations() {
    let four = parse_check_update(&fixture("check-update-dnf4.txt"));
    let rows: Vec<(&str, &str, &str, &str)> = four
        .iter()
        .map(|p| {
            (
                p.name.as_str(),
                p.arch.as_str(),
                p.version.as_str(),
                p.repo.as_str(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (
                "a-very-long-package-name-that-overflows-the-column",
                "noarch",
                "1.2.3-1.fc40",
                "updates"
            ),
            ("brokey", "x86_64", "0.2.0-1.fc40", "brokey"),
            ("gimp", "x86_64", "2.10.38-1.fc40", "updates"),
            ("gimp-libs", "x86_64", "2.10.38-1.fc40", "updates"),
            ("kernel", "x86_64", "6.9.7-200.fc40", "updates"),
            ("vim-enhanced", "x86_64", "2:9.1.496-1.fc40", "updates"),
        ]
    );

    let five = parse_check_update(&fixture("check-upgrade-dnf5.txt"));
    let names: Vec<&str> = five.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        ["brokey", "gimp", "kernel", "new-thing", "vim-enhanced"]
    );

    assert!(parse_check_update("").is_empty());
    assert!(
        parse_check_update(
            "Last metadata expiration check: 0:00:01 ago on Thu 10 Sep 2026 10:00:00 BST.\n"
        )
        .is_empty()
    );
    let dotted = parse_check_update("python3.11.x86_64   3.11.9-2.fc40   updates\n");
    assert_eq!(
        (dotted[0].name.as_str(), dotted[0].arch.as_str()),
        ("python3.11", "x86_64")
    );
}

#[test]
fn rows_parse_in_both_generations_and_from_rpm() {
    let five = parse_rows(&fixture("repoquery-gimp.txt"));
    assert_eq!(five.len(), 7);
    assert_eq!(five[0].name, "gimp");
    assert_eq!(five[0].version, "2.10.36-4.fc40");
    assert_eq!(five[0].repo, "fedora");
    assert_eq!(five[0].build_time, Some(1_712_000_000));
    assert_eq!(five[2].url, None);

    let four = parse_rows(&fixture("repoquery-gimp-dnf4.txt"));
    assert_eq!(four.len(), 3, "blank lines between rows are skipped");
    assert_eq!(four[0].build_time, Some(1_712_003_160));
    assert_eq!(four[1].url, None);
    assert_eq!(four[2].repo, "@System");
    assert_eq!(four[2].download_size, None);

    let rpm = parse_rows(&fixture("rpm-qa.txt"));
    assert_eq!(rpm.len(), 4);
    assert_eq!(rpm[0].repo, "");
    assert_eq!(rpm[0].download_size, None);
    assert_eq!(rpm[0].install_size, Some(8_200_000));
    assert_eq!(rpm[2].name, "gpg-pubkey");
    assert_eq!(rpm[2].url, None);

    assert_eq!(parse_time("2024-04-01 20:26"), Some(1_712_003_160));
}

#[test]
fn the_search_glob_wraps_every_word() {
    assert_eq!(search_glob("gimp"), "*gimp*");
    assert_eq!(search_glob("gnome terminal"), "*gnome*terminal*");
    assert_eq!(search_glob("-x"), "*-x*", "never an option");
}
