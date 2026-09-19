//! chwd's box-drawn listings as this machine prints them (CachyOS, chwd
//! 1.24.1, an Intel and an NVIDIA GPU), in `fixtures/chwd/`, and one live
//! run of the real tool.

#![cfg(unix)]

use brokey_core::sources::linux::chwd::{self, Chwd};
use brokey_core::{Op, PackageKind, PackageRef, Query, Source, SourceKind};

const LIST: &str = include_str!("fixtures/chwd/list.txt");
const INSTALLED: &str = include_str!("fixtures/chwd/list-installed.txt");
const DETAIL: &str = include_str!("fixtures/chwd/list-detail.txt");
const PROFILES: &str = include_str!("fixtures/chwd/profiles.toml");

fn pairs(v: &[(&str, u32)]) -> Vec<(String, u32)> {
    v.iter().map(|(n, p)| (n.to_string(), *p)).collect()
}

fn fact<'a>(facts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    facts
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[test]
fn the_list_has_two_devices_with_their_profiles() {
    let list = chwd::parse_list(LIST);
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].id, "0000:00:02.0");
    assert_eq!(list[0].ids, "0300:8086:3e9b");
    assert_eq!(list[0].text, "VGA compatible controller Intel Corporation");
    assert_eq!(list[0].profiles, pairs(&[("intel", 4), ("fallback", 3)]));
    assert_eq!(list[1].id, "0000:01:00.0");
    assert_eq!(list[1].text, "VGA compatible controller NVIDIA Corporation");
    assert_eq!(
        list[1].profiles,
        pairs(&[
            ("nvidia-open-dkms.prime", 11),
            ("nvidia-open-dkms", 10),
            ("fallback", 3)
        ])
    );
}

#[test]
fn the_installed_listing_is_a_flat_table() {
    assert_eq!(
        chwd::parse_installed(INSTALLED),
        pairs(&[("nvidia-open-dkms.prime", 11), ("intel", 4)])
    );
}

#[test]
fn the_detailed_listing_gives_models_and_descriptions() {
    let detail = chwd::parse_detail(DETAIL);
    assert_eq!(detail.len(), 2);
    assert_eq!(detail[0].ids, "0300:8086:3e9b");
    assert_eq!(
        detail[0].text,
        "VGA compatible controller Intel Corporation CoffeeLake-H GT2 [UHD Graphics 630]"
    );
    assert_eq!(
        detail[0].descriptions,
        vec![
            (
                "intel".to_string(),
                "Mesa open source driver for Intel".to_string()
            ),
            ("fallback".to_string(), "Fallback profile".to_string()),
        ],
        "a profile under both INSTALLED and AVAILABLE is described once"
    );
    assert_eq!(
        detail[1].text,
        "VGA compatible controller NVIDIA Corporation TU106M [GeForce RTX 2070 Mobile]"
    );
    assert_eq!(detail[1].descriptions.len(), 3);
    assert_eq!(
        detail[1].descriptions[0],
        (
            "nvidia-open-dkms.prime".to_string(),
            "Open source NVIDIA drivers for Linux laptops (Latest)".to_string()
        )
    );
}

#[test]
fn the_profile_database_gives_packages() {
    let defs = chwd::parse_profiles_toml(PROFILES);
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "nvidia-open-dkms",
            "nvidia-open-dkms.prime",
            "intel",
            "fallback"
        ]
    );
    let prime = &defs[1];
    assert_eq!(prime.priority, Some(11));
    assert_eq!(
        prime.desc.as_deref(),
        Some("Open source NVIDIA drivers for Linux laptops (Latest)")
    );
    assert!(prime.packages.contains(&"nvidia-prime".to_string()));
    assert!(prime.packages.contains(&"nvidia-utils".to_string()));
    assert_eq!(
        defs[3].packages.len(),
        5,
        "fallback: mesa, lib32-mesa, vulkan-swrast, xf86-video-vesa, lib32-vulkan-swrast"
    );
}

#[test]
fn devices_are_assembled_with_recommended_and_installed_marked() {
    let list = chwd::parse_list(LIST);
    let installed = chwd::parse_installed(INSTALLED);
    let detail = chwd::parse_detail(DETAIL);
    let defs = chwd::parse_profiles_toml(PROFILES);
    let devices = chwd::assemble(&list, &installed, &detail, &defs);
    assert_eq!(devices.len(), 2);

    let intel = &devices[0];
    assert_eq!(intel.id, "0000:00:02.0");
    assert_eq!(intel.name, "CoffeeLake-H GT2 [UHD Graphics 630]");
    assert_eq!(intel.vendor.as_deref(), Some("Intel Corporation"));
    assert_eq!(intel.class.as_deref(), Some("VGA compatible controller"));
    assert_eq!(intel.profiles.len(), 2);
    assert!(intel.profiles[0].installed && intel.profiles[0].recommended);
    assert!(!intel.profiles[1].installed && !intel.profiles[1].recommended);

    let nvidia = &devices[1];
    assert_eq!(nvidia.name, "TU106M [GeForce RTX 2070 Mobile]");
    assert_eq!(nvidia.vendor.as_deref(), Some("NVIDIA Corporation"));
    let prime = &nvidia.profiles[0];
    assert_eq!(prime.id, "nvidia-open-dkms.prime");
    assert!(prime.installed && prime.recommended);
    assert_eq!(
        prime.description.as_deref(),
        Some("Open source NVIDIA drivers for Linux laptops (Latest)")
    );
    assert!(prime.packages.contains(&"nvidia-prime".to_string()));
    let open = &nvidia.profiles[1];
    assert!(!open.installed && !open.recommended);
    assert!(!open.packages.is_empty());
    let fallback = &nvidia.profiles[2];
    assert_eq!(fallback.description.as_deref(), Some("Fallback profile"));
}

#[test]
fn without_the_detailed_listing_the_header_and_database_stand_in() {
    let list = chwd::parse_list(LIST);
    let defs = chwd::parse_profiles_toml(PROFILES);
    let devices = chwd::assemble(&list, &[], &[], &defs);
    assert_eq!(
        devices[1].name,
        "VGA compatible controller NVIDIA Corporation"
    );
    assert_eq!(devices[1].vendor.as_deref(), Some("NVIDIA Corporation"));
    assert_eq!(
        devices[1].profiles[1].description.as_deref(),
        Some("Open source NVIDIA drivers for Linux (Latest)"),
        "the database's description when chwd -d is not there"
    );
    assert!(
        devices
            .iter()
            .flat_map(|d| &d.profiles)
            .all(|p| !p.installed)
    );
    let devices = chwd::assemble(&list, &[], &[], &[]);
    assert_eq!(devices[1].profiles[1].description, None);
    assert!(devices[1].profiles[1].packages.is_empty());
}

#[test]
fn packages_list_each_profile_once_naming_every_device() {
    let list = chwd::parse_list(LIST);
    let devices = chwd::assemble(
        &list,
        &chwd::parse_installed(INSTALLED),
        &chwd::parse_detail(DETAIL),
        &chwd::parse_profiles_toml(PROFILES),
    );
    let packages = chwd::packages(&devices, &list);
    let ids: Vec<&str> = packages.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "intel",
            "fallback",
            "nvidia-open-dkms.prime",
            "nvidia-open-dkms"
        ]
    );
    assert!(
        packages
            .iter()
            .all(|p| p.kind == PackageKind::Driver && p.source == SourceKind::Chwd)
    );

    let fallback = &packages[1];
    assert_eq!(
        fact(&fallback.facts, "Device"),
        Some("CoffeeLake-H GT2 [UHD Graphics 630], TU106M [GeForce RTX 2070 Mobile]")
    );
    assert!(!fallback.installed);
    assert_eq!(fact(&fallback.facts, "Recommended"), Some("No"));

    let prime = &packages[2];
    assert_eq!(
        prime.summary.as_deref(),
        Some("TU106M [GeForce RTX 2070 Mobile] driver profile")
    );
    assert!(prime.installed);
    assert_eq!(fact(&prime.facts, "Priority"), Some("11"));
    assert_eq!(fact(&prime.facts, "Recommended"), Some("Yes"));
    assert!(
        fact(&prime.facts, "Packages")
            .unwrap()
            .contains("nvidia-prime")
    );
    assert_eq!(
        prime.description.as_deref(),
        Some("Open source NVIDIA drivers for Linux laptops (Latest)")
    );
}

#[test]
fn plans_install_and_remove_as_root_and_nothing_else() {
    let source = Chwd::new(&brokey_core::system::from_os_release(
        "ID=cachyos\nID_LIKE=arch\n",
    ));
    let reference = PackageRef {
        source: SourceKind::Chwd,
        id: "nvidia-open-dkms.prime".to_string(),
    };
    let install = source
        .plan(&Op::Install {
            package: reference.clone(),
        })
        .unwrap();
    assert_eq!(install.len(), 1);
    assert_eq!(install[0].command.program, "chwd");
    assert_eq!(
        install[0].command.args,
        vec!["-i", "nvidia-open-dkms.prime"]
    );
    assert!(install[0].needs_root);
    assert_eq!(install[0].weight, 8);
    assert_eq!(
        install[0].title,
        "Installing driver profile nvidia-open-dkms.prime"
    );

    let remove = source
        .plan(&Op::Remove {
            package: reference.clone(),
        })
        .unwrap();
    assert_eq!(remove[0].command.args, vec!["-r", "nvidia-open-dkms.prime"]);
    assert_eq!(
        remove[0].title,
        "Removing driver profile nvidia-open-dkms.prime"
    );

    assert!(
        source
            .plan(&Op::Update { package: reference })
            .unwrap()
            .is_empty()
    );
    assert!(
        source
            .plan(&Op::UpdateAll {
                source: SourceKind::Chwd
            })
            .unwrap()
            .is_empty()
    );
    assert!(
        source
            .plan(&Op::Refresh {
                source: SourceKind::Chwd
            })
            .unwrap()
            .is_empty()
    );

    let bad = PackageRef {
        source: SourceKind::Chwd,
        id: "--force".to_string(),
    };
    assert!(source.plan(&Op::Install { package: bad }).is_err());
}

/// Needs chwd: only CachyOS has it, and this machine is CachyOS.
#[test]
#[ignore]
fn live_chwd_lists_this_machines_profiles() {
    let source = Chwd::new(&brokey_core::system::detect());
    let status = source.status();
    assert!(status.available, "{:?}", status.reason);
    assert!(
        status.detail.as_deref().unwrap_or("").starts_with("chwd "),
        "{:?}",
        status.detail
    );

    let devices = chwd::devices().unwrap();
    assert!(!devices.is_empty());
    for d in &devices {
        assert!(!d.profiles.is_empty(), "{} has profiles", d.name);
        assert!(
            d.profiles.iter().any(|p| p.recommended),
            "{} has a recommended profile",
            d.name
        );
        assert!(d.class.is_some(), "{}", d.name);
    }
    let installed = source.installed().unwrap();
    assert!(!installed.is_empty());
    let found = source.search(&Query::new("nvidia")).unwrap();
    assert!(found.iter().any(|p| p.id.starts_with("nvidia")));
    let details = source.details(&installed[0].id).unwrap();
    assert!(details.installed);
    eprintln!(
        "{} devices, {} profiles installed: {}",
        devices.len(),
        installed.len(),
        installed
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
}
