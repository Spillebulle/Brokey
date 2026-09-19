//! The fwupd source against JSON captured from this machine's fwupdmgr
//! 2.1.7 (`fixtures/fwupd/`), and one live run of the real tool.

#![cfg(unix)]

use brokey_core::sources::linux::fwupd::{self, Fwupd};
use brokey_core::{Op, PackageKind, PackageRef, Query, Source, SourceKind};

const DEVICES: &str = include_str!("fixtures/fwupd/get-devices.json");
const UPDATES: &str = include_str!("fixtures/fwupd/get-updates.json");

fn fact<'a>(facts: &'a [(String, String)], key: &str) -> Option<&'a str> {
    facts
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

#[test]
fn named_versioned_devices_become_installed_firmware() {
    let devices = fwupd::parse_devices(DEVICES).unwrap();
    assert_eq!(devices.len(), 3);
    let packages: Vec<_> = devices.iter().filter_map(fwupd::package).collect();
    assert_eq!(packages.len(), 2, "the nameless GPIO device is skipped");

    let cpu = &packages[0];
    assert_eq!(cpu.source, SourceKind::Fwupd);
    assert_eq!(cpu.kind, PackageKind::Firmware);
    assert_eq!(cpu.id, "4bde70ba4e39b28f9eab1628f9dd6e6244c03027");
    assert_eq!(cpu.name, "Core™ i7-9750H CPU @ 2.60GHz");
    assert_eq!(cpu.developer.as_deref(), Some("Intel"));
    assert!(cpu.installed);
    assert_eq!(cpu.installed_version.as_deref(), Some("0x000000fa"));
    assert_eq!(
        cpu.version.as_deref(),
        Some("0x000000fa"),
        "no release: the version is the installed one"
    );
    assert_eq!(
        cpu.summary.as_deref(),
        Some("Intel Core™ i7-9750H CPU @ 2.60GHz")
    );
    assert_eq!(fact(&cpu.facts, "Updated through"), Some("cpu"));
    assert_eq!(fact(&cpu.facts, "Version format"), Some("hex"));
    assert_eq!(fact(&cpu.facts, "Needs reboot"), Some("No"));
    assert_eq!(fact(&cpu.facts, "Update error"), None);
    // fwupd's internals stay out of the facts: nothing on the page can act
    // on a flag list or a count of GUIDs, and "Needs reboot" is the one
    // flag that matters.
    let keys: Vec<&str> = cpu.facts.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        ["Updated through", "Version format", "Needs reboot"],
        "{keys:?}"
    );

    let bios = &packages[1];
    assert_eq!(bios.name, "System Firmware");
    assert_eq!(bios.developer.as_deref(), Some("Intel(R) Client Systems"));
    assert_eq!(bios.installed_version.as_deref(), Some("158"));
    assert_eq!(
        bios.summary.as_deref(),
        Some("UEFI System Resource Table device (updated via NVRAM)")
    );
    assert_eq!(fact(&bios.facts, "Updated through"), Some("uefi_capsule"));
    assert_eq!(fact(&bios.facts, "Needs reboot"), Some("Yes"));
    assert!(
        fact(&bios.facts, "Update error")
            .unwrap()
            .starts_with("failed to update")
    );
    assert_eq!(fact(&bios.facts, "Flags"), None);
    assert_eq!(fact(&bios.facts, "GUIDs"), None);
    assert_eq!(fact(&bios.facts, "Plugin"), None);
}

#[test]
fn the_updates_listing_gives_one_update() {
    let devices = fwupd::parse_devices(UPDATES).unwrap();
    let updates: Vec<_> = devices.iter().filter_map(fwupd::update).collect();
    assert_eq!(updates.len(), 1);
    let u = &updates[0];
    assert_eq!(u.package.source, SourceKind::Fwupd);
    assert_eq!(u.package.id, "362301da643102b9f38477387e2193e57abaa590");
    assert_eq!(u.name, "UEFI dbx");
    assert_eq!(u.kind, PackageKind::Firmware);
    assert_eq!(u.from.as_deref(), Some("20230301"));
    assert_eq!(u.to, "20260402");
    assert_eq!(u.download_size, Some(24629));
    assert_eq!(u.published, Some(1756771200));
    assert_eq!(
        u.summary.as_deref(),
        Some("UEFI Secure Boot Forbidden Signature Database")
    );
    assert!(!u.is_self);

    let p = fwupd::package(&devices[0]).unwrap();
    assert_eq!(
        p.version.as_deref(),
        Some("20260402"),
        "the version is the one on offer"
    );
    assert_eq!(p.installed_version.as_deref(), Some("20230301"));
    assert!(
        devices[0]
            .newest_release()
            .unwrap()
            .description
            .as_deref()
            .unwrap()
            .starts_with("<p>")
    );
}

#[test]
fn the_report_folds_updates_into_devices() {
    let devices = fwupd::parse_devices(DEVICES).unwrap();
    let updates = fwupd::parse_devices(UPDATES).unwrap();

    let report = fwupd::fold(&devices, &updates);
    assert_eq!(
        report.len(),
        2,
        "every named device, with or without a version"
    );
    assert!(report.iter().all(|d| d.update_version.is_none()));
    assert!(report[1].needs_reboot);
    assert_eq!(report[1].vendor.as_deref(), Some("Intel(R) Client Systems"));

    let report = fwupd::fold(&updates, &updates);
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].version.as_deref(), Some("20230301"));
    assert_eq!(report[0].update_version.as_deref(), Some("20260402"));
    assert_eq!(report[0].update_size, Some(24629));
    assert_eq!(
        report[0].update_summary.as_deref(),
        Some("UEFI Secure Boot Forbidden Signature Database")
    );
    assert!(report[0].needs_reboot);
}

#[test]
fn plans_that_need_no_tool() {
    let source = Fwupd::new(&brokey_core::system::from_os_release(""));
    let reference = PackageRef {
        source: SourceKind::Fwupd,
        id: "abc".to_string(),
    };
    let e = source
        .plan(&Op::Install {
            package: reference.clone(),
        })
        .unwrap_err();
    assert_eq!(
        e.message,
        "Firmware is updated, not installed, and cannot be removed."
    );
    assert!(source.plan(&Op::Remove { package: reference }).is_err());

    let all = source
        .plan(&Op::UpdateAll {
            source: SourceKind::Fwupd,
        })
        .unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].command.program, "fwupdmgr");
    assert_eq!(
        all[0].command.args,
        vec!["update", "-y", "--no-reboot-check"]
    );
    assert!(!all[0].needs_root);
    assert_eq!(all[0].weight, 6);

    let refresh = source
        .plan(&Op::Refresh {
            source: SourceKind::Fwupd,
        })
        .unwrap();
    assert_eq!(refresh[0].command.args, vec!["refresh", "--force"]);
    assert!(!refresh[0].needs_root);

    let one = fwupd::update_step("abc", "TPM");
    assert_eq!(
        one.command.args,
        vec!["update", "-y", "--no-reboot-check", "abc"]
    );
    assert_eq!(one.title, "Updating firmware for TPM");
}

/// Needs fwupdmgr and a running daemon: this machine has both.
#[test]
#[ignore]
fn live_fwupd_lists_this_machines_devices() {
    let source = Fwupd::new(&brokey_core::system::detect());
    let status = source.status();
    assert!(status.available, "{:?}", status.reason);
    assert!(
        status.detail.as_deref().unwrap_or("").starts_with("fwupd "),
        "{:?}",
        status.detail
    );

    let installed = source.installed().unwrap();
    assert!(!installed.is_empty());
    assert!(
        installed
            .iter()
            .all(|p| p.installed && p.installed_version.is_some())
    );
    assert!(installed.iter().all(|p| p.kind == PackageKind::Firmware));

    let by_word = source.search(&Query::new("firmware")).unwrap();
    assert_eq!(
        by_word.len(),
        installed.len(),
        "\"firmware\" finds every device"
    );
    let by_name = source.search(&Query::new(&installed[0].name)).unwrap();
    assert!(by_name.iter().any(|p| p.id == installed[0].id));

    let details = source.details(&installed[0].id).unwrap();
    assert_eq!(details.name, installed[0].name);

    let updates = source.updates().unwrap();
    for u in &updates {
        assert!(
            installed.iter().any(|p| p.id == u.package.id),
            "{} is a listed device",
            u.name
        );
    }

    let plan = source
        .plan(&Op::Update {
            package: installed[0].reference(),
        })
        .unwrap();
    assert_eq!(
        plan[0].title,
        format!("Updating firmware for {}", installed[0].name)
    );

    let devices = fwupd::devices().unwrap();
    assert!(devices.len() >= installed.len());
    eprintln!(
        "{} devices, {} listed as firmware, {} with an update",
        devices.len(),
        installed.len(),
        updates.len()
    );
}
