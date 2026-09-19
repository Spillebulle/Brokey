# Windows foundations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Brokey's workspace builds and tests on Windows, and `brokey installed` lists the real applications on the machine, read straight from the uninstall registry.

**Architecture:** `brokey-core` stays one library. The nine existing sources move to `sources/linux/` and a new `sources/windows/` appears beside them, with `sources::all()` selected by `#[cfg]`. `system.rs` becomes a module with a platform half each. One Windows source is built here, Add/Remove Programs, because it is the only one that needs no network, no elevation and no other tool, so it proves the platform split end to end. Every parser is a pure function over a fixture, exactly as the pacman and apt parsers are.

**Tech Stack:** Rust 2024 edition, stable toolchain, `windows-registry` for the registry, `serde`/`serde_json` for fixtures. No new runtime dependency on Linux.

**Spec:** `docs/superpowers/specs/2026-09-19-windows-support-design.md`

## Global Constraints

- Rust 2024 edition, stable toolchain, `rust-version = "1.88"`. One workspace.
- **A source never runs anything.** `plan()` returns `Step`s; the Runner executes them. A read-only query of a tool is allowed (`system::run`); anything that changes the machine is a Step.
- **`SourceKind` carries every variant on both platforms.** Only `sources::all()` is `#[cfg]`-selected. `crates/brokey/tests/contract.rs` checks every variant against `frontend/src/types.ts` in one run.
- **Copy rules, in Rust strings and TypeScript alike:** British spelling, sentence case, full stops in sentences, no em dashes, no emoji. Errors name what went wrong and what to do.
- **`ProductName` in `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` is never read.** It says `Windows 10 Pro` on Windows 11. Version comes from `CurrentBuild`, where 22000 and above is Windows 11.
- **The install directory is derived from the uninstaller's path**, never taken on faith from `InstallLocation`, which is empty for 221 of the reference machine's 317 entries.
- `cargo test --workspace` needs no network and no Administrator. Network tests are `#[ignore]` and named `live_*`.
- Every commit passes `cargo fmt --all --check`, `cargo clippy --workspace --all-targets` and `cargo test --workspace`.

---

### Task 1: The workspace compiles on Windows

Nothing Windows-specific is added here. The Linux-only code is gated so that `cargo check` succeeds on Windows with no sources at all, which is the precondition for every later task.

**Files:**
- Move: `crates/brokey-core/src/sources/{alpmdb,apt,aur,chwd,dnf,flatpak,fwupd,github,pacman,snap}.rs` to `crates/brokey-core/src/sources/linux/`
- Create: `crates/brokey-core/src/sources/linux/mod.rs`
- Create: `crates/brokey-core/src/sources/windows/mod.rs`
- Modify: `crates/brokey-core/src/sources/mod.rs`
- Modify: `crates/brokey-core/src/system.rs`, `crates/brokey-core/src/launch.rs`, `crates/brokey-core/src/transaction/runner.rs`, `crates/brokey-core/src/transaction/allow.rs`, `crates/brokey-core/src/selfupdate/install.rs`
- Modify: `crates/brokey-helper/src/main.rs`
- Modify: whatever else the build demands, including `crates/brokey/src/`. `Store::launcher` and `Store::launcher_notices` are gated in this task, so their call sites in the Tauri commands need the same treatment. The authority on this task's file list is `cargo check --workspace` on Windows, not this list.
- Test: `crates/brokey-core/src/sources/mod.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces: `sources::all(&SystemInfo, Arc<Client>, Arc<Catalogue>, &Preferences) -> Vec<Box<dyn Source>>`, unchanged in signature, returning an empty vec on Windows until Task 8.

- [ ] **Step 1: Record the build failures**

Run on the Windows machine:

```powershell
cargo check --workspace 2>&1 | Select-String -Pattern '^error' | Select-Object -First 40
```

Expected: many errors. Save the list; it is the work item for this task. The known families are `std::os::unix` imports, `/etc` and `/var` paths, and `PermissionsExt`.

- [ ] **Step 2: Move the Linux sources**

```bash
cd crates/brokey-core/src/sources
mkdir linux windows
git mv alpmdb.rs apt.rs aur.rs chwd.rs dnf.rs flatpak.rs fwupd.rs github.rs pacman.rs snap.rs linux/
```

- [ ] **Step 3: Write `sources/linux/mod.rs`**

```rust
//! The sources a Linux machine has. Built only on Linux; `sources::all`
//! selects between this module and `windows`.

pub mod alpmdb;
pub mod apt;
pub mod aur;
pub mod chwd;
pub mod dnf;
pub mod flatpak;
pub mod fwupd;
pub mod github;
pub mod pacman;
pub mod snap;
```

- [ ] **Step 4: Write `sources/windows/mod.rs`**

```rust
//! The sources a Windows machine has. Built only on Windows; `sources::all`
//! selects between this module and `linux`.
```

- [ ] **Step 5: Rewrite `sources/mod.rs`**

Replace the whole file. The Linux body is the existing `all` moved verbatim into `linux_all`; only the `use` paths change (`aur::` becomes `linux::aur::`, and so on).

```rust
//! One module per source. [`all`] builds them in interface order; each one
//! reports its own availability, so an unavailable source stays in the list
//! and the page can say why.
//!
//! The set is per platform. `SourceKind` is not: every variant exists on
//! every build so the page's types are checked whole. See the spec's
//! "SourceKind keeps every variant on both platforms".

#[cfg(unix)]
pub mod linux;
#[cfg(windows)]
pub mod windows;

use crate::Source;
use crate::appstream::Catalogue;
use crate::http::Client;
use crate::model::SystemInfo;
use std::sync::Arc;

pub fn all(
    system: &SystemInfo,
    client: Arc<Client>,
    catalogue: Arc<Catalogue>,
    preferences: &crate::Preferences,
) -> Vec<Box<dyn Source>> {
    #[cfg(unix)]
    {
        linux_all(system, client, catalogue, preferences)
    }
    #[cfg(windows)]
    {
        let _ = (system, client, catalogue, preferences);
        Vec::new()
    }
}

#[cfg(unix)]
fn linux_all(
    system: &SystemInfo,
    client: Arc<Client>,
    catalogue: Arc<Catalogue>,
    preferences: &crate::Preferences,
) -> Vec<Box<dyn Source>> {
    use linux::*;
    let mut aur = aur::Aur::new(system, client.clone(), catalogue.clone());
    // Automatic stays lazy: the helper is looked for on first use. A named
    // choice is applied now, falling back to what is installed when the
    // named one is not, so the setting never points at a missing program.
    if let Some(choice) = preferences.aur_helper.as_deref() {
        aur = aur.with_helper(aur::Helper::choose(choice));
    }
    let mut flatpak = flatpak::Flatpak::new(system, client.clone());
    if preferences.flatpak_user {
        flatpak.installation = flatpak::Installation::User;
    }
    vec![
        Box::new(pacman::Pacman::new(system, catalogue.clone())),
        Box::new(aur),
        Box::new(flatpak),
        Box::new(snap::Snap::new(system, client.clone())),
        Box::new(apt::Apt::new(system, catalogue.clone())),
        Box::new(dnf::Dnf::new(system, catalogue.clone())),
        Box::new(github::Github::new(
            system,
            client.clone(),
            catalogue.clone(),
        )),
        Box::new(fwupd::Fwupd::new(system)),
        Box::new(chwd::Chwd::new(system)),
    ]
}
```

- [ ] **Step 6: Gate the five Linux-only modules**

In `crates/brokey-core/src/lib.rs`, gate the modules whose whole contents are Linux-only for now. `launch`, and the transaction submodules, keep their names so nothing else moves:

```rust
#[cfg(unix)]
pub mod launch;
```

In `crates/brokey-core/src/transaction/mod.rs`, gate `runner` and `allow` the same way. In `crates/brokey-core/src/selfupdate/mod.rs`, gate `install`. In `crates/brokey-core/src/lib.rs`, gate the `Source::launcher` and `Source::launcher_notice` trait methods:

```rust
    #[cfg(unix)]
    fn launcher(&self, _id: &str) -> Option<launch::Launch> {
        None
    }

    #[cfg(unix)]
    fn launcher_notice(&self) -> Option<String> {
        None
    }
```

Do the same for `Store::launcher` and `Store::launcher_notices`. Plan 2 replaces every one of these gates with a Windows implementation; this task only has to stop them breaking the build.

- [ ] **Step 7: Split `system.rs`**

```bash
cd crates/brokey-core/src
mkdir system
git mv system.rs system/linux.rs
```

`system/windows.rs`, for now:

```rust
//! What machine this is, on Windows. Task 2 fills this in.

use crate::model::SystemInfo;

#[cfg(windows)]
pub fn detect() -> SystemInfo {
    SystemInfo {
        distro_id: "windows".to_string(),
        distro_like: Vec::new(),
        pretty_name: "Windows".to_string(),
        arch: std::env::consts::ARCH.to_string(),
        desktop: None,
        session: None,
    }
}
```

`system/mod.rs`:

```rust
//! What machine this is: the distribution or the Windows edition, the
//! desktop, which tools exist and where things live. Read once and cheap.
//!
//! Both halves are compiled on both platforms. Only the functions that
//! actually touch the machine are selected by `#[cfg]`; the parsers that
//! turn what was read into a `SystemInfo` are pure and are built
//! everywhere, so each one's tests run on either machine and the page's
//! contract test can check both shapes in one run.

pub mod linux;
pub mod windows;

#[cfg(unix)]
pub use linux::{detect, is_executable, which};
#[cfg(windows)]
pub use windows::detect;

// Pure on both platforms, so it is always in scope.
pub use linux::from_os_release;
```

Nothing else is re-exported yet. Task 2 adds `pub use windows::from_registry_version;` and Task 4 adds `which` to the Windows line, each when the function it names exists. Re-exporting a name before its task has written it will not compile.

Inside `system/linux.rs`, put `#[cfg(unix)]` on `detect`, `which` and `is_executable`, which use `/etc/os-release` and `PermissionsExt`. Leave `from_os_release` ungated: it is a pure text parser and nothing in it is Linux-specific.

`run` is portable. Move it from `system/linux.rs` to `system/mod.rs` so `system::run` resolves on both platforms.

- [ ] **Step 8: Gate the Linux-only integration tests**

Every file in `crates/brokey-core/tests/` that reads a Linux fixture or names a Linux source gets `#![cfg(unix)]` as its first line. Do the same for `crates/brokey/tests/release.rs` only if it fails; it should not.

- [ ] **Step 9: Make `brokey-helper` compile on Windows**

`crates/brokey-helper/src/main.rs` keeps its Linux body and gains a Windows stub. Plan 2 replaces the stub with the named-pipe helper.

```rust
#[cfg(windows)]
fn main() {
    eprintln!("This build of brokey-helper does not run on Windows yet.");
    std::process::exit(1);
}
```

Wrap the existing `main` and everything it uses in `#[cfg(unix)]`.

- [ ] **Step 10: Write the test that the Windows source list is empty and honest**

Add to the bottom of `crates/brokey-core/src/sources/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    /// Until Task 8 there is no Windows source. The point of the test is
    /// that `all` answers rather than panicking or being absent, so the
    /// application and the text mode both run on Windows from here on.
    #[cfg(windows)]
    #[test]
    fn windows_has_no_sources_yet() {
        let system = crate::system::detect();
        let sources = super::all(
            &system,
            crate::http::Client::shared(),
            crate::appstream::Catalogue::load_system(&system),
            &crate::Preferences::default(),
        );
        assert!(sources.is_empty());
    }
}
```

- [ ] **Step 11: Run the checks**

```powershell
cargo fmt --all --check; if ($?) { cargo clippy --workspace --all-targets }; if ($?) { cargo test --workspace }
```

Expected: all three pass on Windows. Then confirm nothing broke on Linux by pushing the branch and reading the `rust ubuntu-22.04` job, or by running the same three commands there.

- [ ] **Step 12: Commit**

```bash
git add -A
git commit -m "Windows: split the sources and the system module by platform

The nine Linux sources move to sources/linux/ and sources::all is selected
by cfg. launch, the runner, the helper's closed list and the self-update
detection are gated until plan 2 gives each a Windows half. Nothing is
added for Windows yet: the point is that the workspace builds and tests
there, which nothing after this can be done without."
```

---

### Task 2: Platform, and the Windows SystemInfo that does not trust ProductName

**Files:**
- Modify: `crates/brokey-core/src/model.rs`
- Modify: `crates/brokey-core/src/system/windows.rs`
- Modify: `crates/brokey-core/src/system/linux.rs`
- Modify: `crates/brokey/tests/contract.rs`
- Modify: `frontend/src/types.ts`

**Interfaces:**
- Consumes: `sources::all` from Task 1.
- Produces:
  - `model::Platform` with variants `Linux` and `Windows`, serialised lower-case.
  - `SystemInfo.platform: Platform`.
  - `system::windows::RegistryVersion { product_name: Option<String>, edition_id: Option<String>, display_version: Option<String>, current_build: Option<String>, ubr: Option<u32> }`
  - `system::windows::from_registry_version(v: &RegistryVersion) -> SystemInfo`

- [ ] **Step 1: Write the failing tests**

Add to `crates/brokey-core/src/system/windows.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn version(product: &str, build: &str) -> RegistryVersion {
        RegistryVersion {
            product_name: Some(product.to_string()),
            edition_id: Some("Professional".to_string()),
            display_version: Some("26H1".to_string()),
            current_build: Some(build.to_string()),
            ubr: Some(2738),
        }
    }

    /// The reference machine is Windows 11 and its ProductName says
    /// Windows 10 Pro. Microsoft never updated the value. Software that
    /// reads it reports the wrong operating system, so this one does not.
    #[test]
    fn the_version_comes_from_the_build_not_from_product_name() {
        let info = from_registry_version(&version("Windows 10 Pro", "28120"));
        assert_eq!(info.pretty_name, "Windows 11 Pro 26H1 (build 28120.2738)");
        assert_eq!(info.platform, crate::model::Platform::Windows);
        assert_eq!(info.distro_id, "windows");
        assert!(info.distro_like.is_empty());
    }

    #[test]
    fn a_build_below_22000_is_windows_10() {
        let info = from_registry_version(&version("Windows 10 Pro", "19045"));
        assert_eq!(info.pretty_name, "Windows 10 Pro 26H1 (build 19045.2738)");
    }

    /// Nothing in the key is guaranteed to be there. A machine that answers
    /// none of it still gets a name rather than an empty string.
    #[test]
    fn a_registry_that_says_nothing_still_names_the_machine() {
        let info = from_registry_version(&RegistryVersion {
            product_name: None,
            edition_id: None,
            display_version: None,
            current_build: None,
            ubr: None,
        });
        assert_eq!(info.pretty_name, "Windows");
    }

    /// UBR is the patch level and is missing on some installations.
    #[test]
    fn a_missing_ubr_leaves_the_build_bare() {
        let info = from_registry_version(&RegistryVersion {
            ubr: None,
            ..version("Windows 10 Pro", "28120")
        });
        assert_eq!(info.pretty_name, "Windows 11 Pro 26H1 (build 28120)");
    }

    /// EditionID is a bare word: Professional, Core, Enterprise. The name
    /// uses the word people know.
    #[test]
    fn edition_ids_become_the_words_people_use() {
        let core = RegistryVersion {
            edition_id: Some("Core".to_string()),
            ..version("Windows 10 Pro", "28120")
        };
        assert_eq!(
            from_registry_version(&core).pretty_name,
            "Windows 11 Home 26H1 (build 28120.2738)"
        );
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test -p brokey-core --lib system::windows
```

Expected: FAIL, `cannot find struct RegistryVersion`.

These tests are not `#[cfg(windows)]`. `from_registry_version` is a pure
function and is compiled on both platforms, so the naming rule is checked on
the Linux runners too. Only `detect` and `read_registry_version` are gated.

- [ ] **Step 3: Add `Platform` to `model.rs`**

Beside `SystemInfo`:

```rust
/// Which operating system this is. The page needs it because the nav and
/// the status bar differ; it must never be inferred from which sources
/// happen to be present.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Linux,
    Windows,
}
```

Add `pub platform: Platform,` to `SystemInfo`, and set `platform: Platform::Linux` in `system/linux.rs`'s `from_os_release`.

- [ ] **Step 4: Write `system/windows.rs`**

```rust
//! What machine this is, on Windows.
//!
//! `ProductName` in `CurrentVersion` is not read. On the reference machine,
//! which is Windows 11, it says "Windows 10 Pro": Microsoft never updated
//! the value and a great deal of software reports the wrong operating
//! system because of it. `CurrentBuild` is the truth, and 22000 is where
//! Windows 11 begins.

use crate::model::{Platform, SystemInfo};

/// The values read from `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`.
/// Kept apart from the reading so the naming is a pure function with tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RegistryVersion {
    /// Read for completeness and never used. See the module note.
    pub product_name: Option<String>,
    pub edition_id: Option<String>,
    pub display_version: Option<String>,
    pub current_build: Option<String>,
    pub ubr: Option<u32>,
}

/// The first build number of Windows 11.
const WINDOWS_11: u32 = 22000;

#[cfg(windows)]
pub fn detect() -> SystemInfo {
    from_registry_version(&read_registry_version())
}

/// The pure half of [`detect`], so it can be tested against a machine that
/// is not this one.
pub fn from_registry_version(v: &RegistryVersion) -> SystemInfo {
    let build: Option<u32> = v.current_build.as_deref().and_then(|b| b.parse().ok());
    let mut name = String::from("Windows");
    if let Some(build) = build {
        name.push(' ');
        name.push_str(if build >= WINDOWS_11 { "11" } else { "10" });
    }
    if let Some(edition) = v.edition_id.as_deref().map(edition_name) {
        name.push(' ');
        name.push_str(edition);
    }
    if let Some(display) = v.display_version.as_deref() {
        name.push(' ');
        name.push_str(display);
    }
    if let Some(build) = build {
        match v.ubr {
            Some(ubr) => name.push_str(&format!(" (build {build}.{ubr})")),
            None => name.push_str(&format!(" (build {build})")),
        }
    }
    SystemInfo {
        distro_id: "windows".to_string(),
        distro_like: Vec::new(),
        pretty_name: name,
        arch: std::env::consts::ARCH.to_string(),
        desktop: None,
        session: None,
        platform: Platform::Windows,
    }
}

/// `EditionID` is a bare word. These are the ones a desktop machine has;
/// anything else is shown as it is written, which is better than dropping it.
fn edition_name(edition_id: &str) -> &str {
    match edition_id {
        "Core" | "CoreN" | "CoreSingleLanguage" => "Home",
        "Professional" | "ProfessionalN" => "Pro",
        "ProfessionalWorkstation" => "Pro for Workstations",
        "Enterprise" | "EnterpriseN" => "Enterprise",
        "Education" | "EducationN" => "Education",
        other => other,
    }
}

#[cfg(windows)]
fn read_registry_version() -> RegistryVersion {
    let Ok(key) = windows_registry::LOCAL_MACHINE
        .open(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
    else {
        return RegistryVersion::default();
    };
    RegistryVersion {
        product_name: key.get_string("ProductName").ok(),
        edition_id: key.get_string("EditionID").ok(),
        display_version: key.get_string("DisplayVersion").ok(),
        current_build: key.get_string("CurrentBuild").ok(),
        ubr: key.get_u32("UBR").ok(),
    }
}
```

- [ ] **Step 5: Add the dependency**

```powershell
cargo add --package brokey-core --target 'cfg(windows)' windows-registry
```

- [ ] **Step 6: Run the tests to verify they pass**

```powershell
cargo test -p brokey-core --lib system::windows
```

Expected: PASS, five tests.

- [ ] **Step 7: Teach the page's contract about the new field**

In `frontend/src/types.ts`, add to the `SystemInfo` interface:

```typescript
  platform: "linux" | "windows";
```

In `crates/brokey/tests/contract.rs`, the existing sample at line 617 is
`c.value("SystemInfo", &brokey_core::system::from_os_release("ID=cachyos\nID_LIKE=arch\n"))`.
Because both parsers are now compiled everywhere, check both shapes rather
than only the one this runner happens to be. Replace that call with:

```rust
    c.value(
        "SystemInfo",
        &brokey_core::system::from_os_release("ID=cachyos\nID_LIKE=arch\n"),
    );
    // The Windows shape is checked on every runner, not only a Windows one.
    // A field the Linux half never fills is exactly the kind of thing that
    // would otherwise reach the page unchecked.
    c.value(
        "SystemInfo",
        &brokey_core::system::from_registry_version(
            &brokey_core::system::windows::RegistryVersion {
                product_name: Some("Windows 10 Pro".into()),
                edition_id: Some("Professional".into()),
                display_version: Some("26H1".into()),
                current_build: Some("28120".into()),
                ubr: Some(2738),
            },
        ),
    );
```

- [ ] **Step 8: Run the contract test**

```powershell
cargo test -p brokey --test contract
```

Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "Windows: name the machine from the build number, never ProductName

ProductName in CurrentVersion says Windows 10 Pro on the reference machine,
which is Windows 11. Microsoft never updated it. The version comes from
CurrentBuild, where 22000 and above is 11, and the name is composed from
EditionID, DisplayVersion and the build with its UBR. SystemInfo gains a
platform field so the page can tell without guessing from the source list."
```

---

### Task 3: SourceKind gains the Windows variants

**Files:**
- Modify: `crates/brokey-core/src/model.rs:14-72`
- Modify: `frontend/src/types.ts`
- Modify: `crates/brokey/tests/contract.rs`

**Interfaces:**
- Consumes: Task 2's contract-test changes.
- Produces: `SourceKind::{Winget, Arp, Choco, Scoop, Msix, Features}`, `SourceKind::ALL` of length 15, `id()` and `label()` for each, and `parse()` accepting the new words.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module in `crates/brokey-core/src/model.rs`, or create one at the bottom of the file if there is none:

```rust
#[cfg(test)]
mod source_kind_tests {
    use super::SourceKind;

    /// Every variant exists on every platform so the page's types can be
    /// checked whole in one run. 0.1.0 shipped a self-update button that
    /// never appeared because two sides spelt a tag differently and each
    /// tested its own.
    #[test]
    fn every_kind_round_trips_through_its_id() {
        assert_eq!(SourceKind::ALL.len(), 15);
        for kind in SourceKind::ALL {
            assert_eq!(SourceKind::parse(kind.id()), Some(kind), "{kind:?}");
        }
    }

    #[test]
    fn the_windows_kinds_are_spelt_as_the_settings_file_spells_them() {
        assert_eq!(SourceKind::parse("winget"), Some(SourceKind::Winget));
        assert_eq!(SourceKind::parse("arp"), Some(SourceKind::Arp));
        assert_eq!(SourceKind::parse("choco"), Some(SourceKind::Choco));
        assert_eq!(SourceKind::parse("scoop"), Some(SourceKind::Scoop));
        assert_eq!(SourceKind::parse("msix"), Some(SourceKind::Msix));
        assert_eq!(SourceKind::parse("features"), Some(SourceKind::Features));
    }

    /// Badges are neutral words a person recognises, sentence case.
    #[test]
    fn the_windows_badges_read_as_people_name_them() {
        assert_eq!(SourceKind::Winget.label(), "winget");
        assert_eq!(SourceKind::Arp.label(), "Installed");
        assert_eq!(SourceKind::Choco.label(), "Chocolatey");
        assert_eq!(SourceKind::Scoop.label(), "Scoop");
        assert_eq!(SourceKind::Msix.label(), "Store");
        assert_eq!(SourceKind::Features.label(), "Features");
    }

    #[test]
    fn no_two_kinds_share_an_id() {
        let mut ids: Vec<&str> = SourceKind::ALL.iter().map(|k| k.id()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test -p brokey-core --lib source_kind_tests
```

Expected: FAIL, `no variant named Winget`.

- [ ] **Step 3: Add the variants**

In `crates/brokey-core/src/model.rs`, extend the enum, `ALL`, `id()` and `label()`. The Linux arms are unchanged; these are added after `Chwd`:

```rust
    Winget,
    Arp,
    Choco,
    Scoop,
    Msix,
    Features,
```

`ALL` becomes `[SourceKind; 15]` with the six appended in that order. In `id()`:

```rust
            Self::Winget => "winget",
            Self::Arp => "arp",
            Self::Choco => "choco",
            Self::Scoop => "scoop",
            Self::Msix => "msix",
            Self::Features => "features",
```

In `label()`:

```rust
            Self::Winget => "winget",
            // What Add/Remove Programs holds is everything the machine has,
            // whoever put it there. "Installed" is what a person calls it;
            // "ARP" is a registry key name and means nothing to them.
            Self::Arp => "Installed",
            Self::Choco => "Chocolatey",
            Self::Scoop => "Scoop",
            Self::Msix => "Store",
            Self::Features => "Features",
```

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test -p brokey-core --lib source_kind_tests
```

Expected: PASS, four tests.

- [ ] **Step 5: Extend the page's union type**

In `frontend/src/types.ts`, add the six words to the `SourceKind` union.

- [ ] **Step 6: Run the contract test and the page's checks**

```powershell
cargo test -p brokey --test contract
cd frontend; npm run lint; npm run check:design; cd ..
```

Expected: all pass. If `contract.rs` fails, the Rust and TypeScript spellings differ, which is the whole point of the test.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "SourceKind carries the Windows variants on every platform

Only sources::all is selected by cfg. If the enum shrank per platform the
serde words and types.ts would differ per build and contract.rs could only
ever check the half it was compiled for, which is exactly how 0.1.0 shipped
a button that never appeared."
```

---

### Task 4: `which` on Windows

`system::which` is Linux-only because it tests the executable bit. Windows has no such bit and uses `PATHEXT` instead. Later tasks ask whether `choco.exe` and `winget.exe` exist, so this comes first.

**Files:**
- Modify: `crates/brokey-core/src/system/windows.rs`
- Modify: `crates/brokey-core/src/system/mod.rs`

**Interfaces:**
- Consumes: Task 2's `system/windows.rs`.
- Produces: `system::which(name: &str) -> Option<PathBuf>`, available on both platforms with the same signature, and `system::windows::which_in(name: &str, path: &str, pathext: &str) -> Option<PathBuf>` for tests.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/brokey-core/src/system/windows.rs`:

```rust
    /// `PATHEXT` supplies the extension's spelling and is conventionally
    /// upper case, while the file on disk is usually lower case, so the
    /// path `which_in` builds and the path the test wrote can differ in
    /// spelling while naming one file. Canonicalising both is how the test
    /// says "the same file" rather than "the same string". Comparing the
    /// `PathBuf`s directly fails on Windows for that reason alone.
    fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
        match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
    }

    /// The directories and the extensions come in as text so the search is
    /// a pure function. Nothing here touches the real PATH.
    #[test]
    fn a_bare_name_finds_the_executable_with_an_extension() {
        let dir = tempdir();
        std::fs::write(dir.join("choco.exe"), b"").unwrap();
        let found = which_in("choco", dir.to_str().unwrap(), ".COM;.EXE;.BAT");
        assert!(same_file(&found.expect("it is found"), &dir.join("choco.exe")));
    }

    /// PATHEXT is tried in its own order, so a .com wins over a .exe when
    /// it comes first, which is what the shell does. The assertion still
    /// discriminates: `thing.com` and `thing.exe` are two files and
    /// canonicalise differently.
    #[test]
    fn pathext_is_tried_in_order() {
        let dir = tempdir();
        std::fs::write(dir.join("thing.exe"), b"").unwrap();
        std::fs::write(dir.join("thing.com"), b"").unwrap();
        let found = which_in("thing", dir.to_str().unwrap(), ".COM;.EXE");
        assert!(same_file(&found.expect("it is found"), &dir.join("thing.com")));
    }

    /// A name that already carries an extension is taken as it is.
    #[test]
    fn a_name_with_an_extension_is_not_extended_again() {
        let dir = tempdir();
        std::fs::write(dir.join("winget.exe"), b"").unwrap();
        let found = which_in("winget.exe", dir.to_str().unwrap(), ".EXE");
        assert_eq!(found, Some(dir.join("winget.exe")));
    }

    #[test]
    fn a_name_that_is_not_there_is_not_found() {
        let dir = tempdir();
        assert_eq!(which_in("absent", dir.to_str().unwrap(), ".EXE"), None);
    }

    /// Directories earlier in PATH win.
    #[test]
    fn the_first_directory_on_the_path_wins() {
        let first = tempdir();
        let second = tempdir();
        std::fs::write(first.join("dup.exe"), b"").unwrap();
        std::fs::write(second.join("dup.exe"), b"").unwrap();
        let path = format!("{};{}", first.display(), second.display());
        let found = which_in("dup", &path, ".EXE");
        assert!(same_file(&found.expect("it is found"), &first.join("dup.exe")));
    }

    /// A unique directory under the system temporary directory, removed by
    /// the operating system rather than by the test, so a failing test
    /// leaves its evidence behind.
    fn tempdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "brokey-which-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test -p brokey-core --lib system::windows
```

Expected: FAIL, `cannot find function which_in`.

- [ ] **Step 3: Implement**

Add to `crates/brokey-core/src/system/windows.rs`:

```rust
use std::path::{Path, PathBuf};

/// The first directory on `PATH` holding an executable of that name.
/// Windows has no executable bit: a name without an extension is tried
/// against each extension in `PATHEXT`, in that order, the way the shell
/// does it.
#[cfg(windows)]
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    let pathext =
        std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    which_in(name, &path, &pathext)
}

/// The pure half of [`which`], taking the two environment variables as
/// text. Compiled on both platforms so its tests run on both; only the
/// wrapper that reads the real environment is Windows-only.
pub fn which_in(name: &str, path: &str, pathext: &str) -> Option<PathBuf> {
    let has_extension = Path::new(name).extension().is_some();
    for dir in path.split(';').filter(|d| !d.is_empty()) {
        let base = Path::new(dir).join(name);
        if has_extension {
            if base.is_file() {
                return Some(base);
            }
            continue;
        }
        for extension in pathext.split(';').filter(|e| !e.is_empty()) {
            let candidate = Path::new(dir).join(format!("{name}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test -p brokey-core --lib system::windows
```

Expected: PASS, ten tests (five from Task 2, five here). They pass on Linux too, because everything under test here is pure.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "Windows: find a program on PATH through PATHEXT

There is no executable bit to test. A bare name is tried against each
extension in PATHEXT in its own order, which is what the shell does, and
the search is a pure function over the two variables so it has tests that
do not depend on what happens to be installed."
```

---

### Task 5: The Add/Remove Programs filter

The registry holds 343 keys on the reference machine and roughly 157 applications. The filter decides which, and like `group.rs` it is a pure function with a fixture where each heuristic fires and one where it must not.

**Files:**
- Create: `crates/brokey-core/src/sources/windows/arp.rs`
- Create: `crates/brokey-core/tests/fixtures/windows/arp/entries.json`
- Modify: `crates/brokey-core/src/sources/windows/mod.rs`
- Modify: `crates/brokey-core/src/sources/mod.rs` (drop the `#[cfg(windows)]` above `pub mod windows;`)

**Interfaces:**
- Consumes: Task 3's `SourceKind::Arp`.
- Produces:
  - `sources::windows::arp::Hive` with variants `Machine`, `Machine32`, `User`, and `Hive::id(self) -> &'static str` giving `"HKLM"`, `"HKLM32"`, `"HKCU"`.
  - `sources::windows::arp::RawEntry` with the fields listed in Step 3, `Deserialize` so the fixture loads.
  - `sources::windows::arp::is_application(e: &RawEntry) -> bool`

- [ ] **Step 1: Write the fixture**

`crates/brokey-core/tests/fixtures/windows/arp/entries.json`. Every entry is taken from the reference machine except where a heuristic needs one that the machine does not have.

```json
[
  {
    "hive": "machine",
    "key_name": "7-Zip",
    "display_name": "7-Zip 26.00 (x64)",
    "display_version": "26.00",
    "publisher": "Igor Pavlov",
    "install_location": "C:\\Program Files\\7-Zip\\",
    "uninstall_string": "\"C:\\Program Files\\7-Zip\\Uninstall.exe\"",
    "quiet_uninstall_string": "\"C:\\Program Files\\7-Zip\\Uninstall.exe\" /S",
    "windows_installer": null,
    "display_icon": "C:\\Program Files\\7-Zip\\7zFM.exe",
    "system_component": null,
    "parent_key_name": null,
    "release_type": null,
    "estimated_size": 5133,
    "url_info_about": "https://www.7-zip.org/"
  },
  {
    "hive": "machine",
    "key_name": "Obsidian",
    "display_name": "Obsidian",
    "display_version": "1.8.9",
    "publisher": "Obsidian",
    "install_location": null,
    "uninstall_string": "\"C:\\Program Files\\Obsidian\\Uninstall Obsidian.exe\" /allusers",
    "quiet_uninstall_string": "\"C:\\Program Files\\Obsidian\\Uninstall Obsidian.exe\" /allusers /S",
    "windows_installer": null,
    "display_icon": "C:\\Program Files\\Obsidian\\Obsidian.exe,0",
    "system_component": null,
    "parent_key_name": null,
    "release_type": null,
    "estimated_size": null,
    "url_info_about": null
  },
  {
    "hive": "machine",
    "key_name": "{90160000-008C-0000-1000-0000000FF1CE}",
    "display_name": "Office 16 Click-to-Run Licensing Component",
    "display_version": "16.0.18827.20202",
    "publisher": "Microsoft Corporation",
    "install_location": null,
    "uninstall_string": null,
    "quiet_uninstall_string": null,
    "windows_installer": null,
    "display_icon": null,
    "system_component": 1,
    "parent_key_name": null,
    "release_type": null,
    "estimated_size": null,
    "url_info_about": null
  },
  {
    "hive": "machine",
    "key_name": "KB5034441",
    "display_name": "Update for Windows",
    "display_version": null,
    "publisher": "Microsoft Corporation",
    "install_location": null,
    "uninstall_string": null,
    "quiet_uninstall_string": null,
    "windows_installer": null,
    "display_icon": null,
    "system_component": null,
    "parent_key_name": null,
    "release_type": "Security Update",
    "estimated_size": null,
    "url_info_about": null
  },
  {
    "hive": "machine",
    "key_name": "SomeSuiteComponent",
    "display_name": "A component of a suite",
    "display_version": "1.0",
    "publisher": "Example Ltd",
    "install_location": null,
    "uninstall_string": "\"C:\\Program Files\\Example\\unins000.exe\"",
    "quiet_uninstall_string": null,
    "windows_installer": null,
    "display_icon": null,
    "system_component": null,
    "parent_key_name": "TheSuite",
    "release_type": null,
    "estimated_size": null,
    "url_info_about": null
  },
  {
    "hive": "machine",
    "key_name": "NoName",
    "display_name": null,
    "display_version": "3.2",
    "publisher": null,
    "install_location": null,
    "uninstall_string": null,
    "quiet_uninstall_string": null,
    "windows_installer": null,
    "display_icon": null,
    "system_component": null,
    "parent_key_name": null,
    "release_type": null,
    "estimated_size": null,
    "url_info_about": null
  },
  {
    "hive": "machine32",
    "key_name": "{ca4f4d7f-6b8b-4f8d-9d2f-000000000001}",
    "display_name": "Windows Driver Package - Arduino LLC (www.arduino.cc) Arduino USB Driver (11/24/2015 1.2.3.0)",
    "display_version": "11/24/2015 1.2.3.0",
    "publisher": "Arduino LLC (www.arduino.cc)",
    "install_location": null,
    "uninstall_string": "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE /u C:\\WINDOWS\\System32\\DriverStore\\FileRepository\\arduino.inf_amd64_6cb1adf1bc8e1d48\\arduino.inf",
    "quiet_uninstall_string": null,
    "windows_installer": null,
    "display_icon": "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE,0",
    "system_component": null,
    "parent_key_name": null,
    "release_type": null,
    "estimated_size": null,
    "url_info_about": null
  },
  {
    "hive": "user",
    "key_name": "{6f320b93-ee3c-4826-85e0-000000000002}",
    "display_name": "A per-user application",
    "display_version": "2.4.1",
    "publisher": "Example Ltd",
    "install_location": "C:\\Users\\test\\AppData\\Local\\Example",
    "uninstall_string": "MsiExec.exe /I{6F320B93-EE3C-4826-85E0-000000000002}",
    "quiet_uninstall_string": null,
    "windows_installer": 1,
    "display_icon": null,
    "system_component": null,
    "parent_key_name": null,
    "release_type": null,
    "estimated_size": 40960,
    "url_info_about": null
  },
  {
    "hive": "machine32",
    "key_name": "Notepad-plus-plus",
    "display_name": "Notepad++ (32-bit x86)",
    "display_version": "8.6.2",
    "publisher": "Notepad++ Team",
    "install_location": "C:\\Program Files (x86)\\Notepad++",
    "uninstall_string": "\"C:\\Program Files (x86)\\Notepad++\\uninstall.exe\"",
    "quiet_uninstall_string": null,
    "windows_installer": null,
    "display_icon": "C:\\Program Files (x86)\\Notepad++\\notepad++.exe",
    "system_component": null,
    "parent_key_name": null,
    "release_type": null,
    "estimated_size": null,
    "url_info_about": null
  }
]
```

That is nine entries, of which five survive `is_application`: 7-Zip, Obsidian, the Arduino driver package, the per-user application and Notepad++. The four that do not are the system component, the security update, the child of a suite and the nameless key, one for each rule in the filter. Notepad++ is there because without it the fixture had no ordinary application falling to the interactive removal route: the only entry reaching it was the driver, and a rule tested by a single example of a single kind is not tested.

- [ ] **Step 2: Write the failing tests**

Create `crates/brokey-core/src/sources/windows/arp.rs` with only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<RawEntry> {
        let text = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/windows/arp/entries.json"
        ))
        .expect("the fixture is checked in beside the source");
        serde_json::from_str(&text).expect("the fixture parses")
    }

    fn named<'a>(entries: &'a [RawEntry], name: &str) -> &'a RawEntry {
        entries
            .iter()
            .find(|e| e.display_name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name} is not in the fixture"))
    }

    /// An ordinary application with a name and an uninstaller is kept.
    #[test]
    fn a_plain_application_is_kept() {
        let entries = fixture();
        assert!(is_application(named(&entries, "7-Zip 26.00 (x64)")));
        assert!(is_application(named(&entries, "Obsidian")));
        assert!(is_application(named(&entries, "A per-user application")));
    }

    /// 160 of the reference machine's 317 named entries are
    /// SystemComponent=1: runtimes and redistributables that Settings
    /// itself hides. A store that showed them would be listing plumbing.
    #[test]
    fn a_system_component_is_dropped() {
        let entries = fixture();
        assert!(!is_application(named(
            &entries,
            "Office 16 Click-to-Run Licensing Component"
        )));
    }

    /// Updates and hotfixes are not software a person installed.
    #[test]
    fn an_update_is_dropped() {
        let entries = fixture();
        assert!(!is_application(named(&entries, "Update for Windows")));
    }

    /// A ParentKeyName means the entry is a part of something else that
    /// has its own entry, so listing it would show one application twice.
    #[test]
    fn a_child_of_another_entry_is_dropped() {
        let entries = fixture();
        assert!(!is_application(named(&entries, "A component of a suite")));
    }

    /// Nothing without a name can be drawn.
    #[test]
    fn an_entry_with_no_display_name_is_dropped() {
        let entries = fixture();
        let nameless = entries
            .iter()
            .find(|e| e.key_name == "NoName")
            .expect("the fixture has one");
        assert!(!is_application(nameless));
    }

    /// Driver packages are kept rather than dropped. They are real things
    /// on the machine and the second spec will want them listed. There is no
    /// driver rule in the filter and there must not be one: a package like
    /// this is kept because it has a name, no SystemComponent, no
    /// ParentKeyName and no ReleaseType, exactly like any other entry. What
    /// this test guards is that nobody later reaches for a name prefix such
    /// as "Windows Driver Package", which would drop every driver on a
    /// machine running in another language.
    #[test]
    fn a_driver_package_is_kept() {
        let entries = fixture();
        let driver = entries
            .iter()
            .find(|e| e.display_name.as_deref().is_some_and(|n| n.contains("Arduino")))
            .expect("the fixture has one");
        assert!(is_application(driver));
    }

    /// The whole fixture at once, so a heuristic added later cannot quietly
    /// change the answer for everything.
    #[test]
    fn the_fixture_yields_exactly_the_five_applications() {
        let entries = fixture();
        let kept: Vec<&str> = entries
            .iter()
            .filter(|e| is_application(e))
            .filter_map(|e| e.display_name.as_deref())
            .collect();
        assert_eq!(kept.len(), 5, "{kept:?}");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

```powershell
cargo test -p brokey-core --lib arp
```

Expected: FAIL, `cannot find type RawEntry`.

- [ ] **Step 4: Implement `RawEntry`, `Hive` and `is_application`**

At the top of `crates/brokey-core/src/sources/windows/arp.rs`:

```rust
//! Add/Remove Programs: the three uninstall keys, which between them are
//! the only record of the software a Windows machine has that no package
//! manager put there.
//!
//! This source answers `installed` and `plan(Remove)` and nothing else.
//! The registry knows what is on the machine and has no notion of a newer
//! version, so it never searches and never reports an update.
//!
//! Everything here except [`read`] is a pure function of [`RawEntry`], so
//! it is tested against a fixture rather than against whatever happens to
//! be installed on the machine running the tests.

use serde::Deserialize;

/// Which of the three uninstall keys an entry came from. It is part of the
/// package id, because the same key name can appear in more than one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Hive {
    /// `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall`
    Machine,
    /// The same under `WOW6432Node`: 32-bit software on a 64-bit machine.
    Machine32,
    /// `HKCU\...`: installed for this user, and removable without elevating.
    User,
}

impl Hive {
    pub fn id(self) -> &'static str {
        match self {
            Hive::Machine => "HKLM",
            Hive::Machine32 => "HKLM32",
            Hive::User => "HKCU",
        }
    }

    /// Removing something the whole machine has needs Administrator;
    /// removing something only this user has does not.
    pub fn needs_elevation(self) -> bool {
        !matches!(self, Hive::User)
    }
}

/// One uninstall key, as its values stand. Nothing is interpreted here.
#[derive(Clone, Debug, Deserialize)]
pub struct RawEntry {
    pub hive: Hive,
    /// The key's own name: an MSI ProductCode in braces, or a word the
    /// installer chose. Unique within its hive.
    pub key_name: String,
    pub display_name: Option<String>,
    pub display_version: Option<String>,
    pub publisher: Option<String>,
    /// Empty for 221 of the reference machine's 317 entries. Never trusted
    /// on its own; see `install_dir`.
    pub install_location: Option<String>,
    pub uninstall_string: Option<String>,
    pub quiet_uninstall_string: Option<String>,
    /// `1` when the Windows Installer owns this product. It is the flag
    /// Windows itself sets, and the only trustworthy way to tell a real MSI
    /// from an entry that merely happens to be keyed by a GUID, which the
    /// driver packages on the reference machine are.
    pub windows_installer: Option<u32>,
    /// A path to an `.exe` or `.ico`, optionally followed by `,` and a
    /// resource index.
    pub display_icon: Option<String>,
    pub system_component: Option<u32>,
    pub parent_key_name: Option<String>,
    /// "Security Update", "Update", "Hotfix" for the things Windows
    /// installed itself.
    pub release_type: Option<String>,
    /// Kilobytes, as the registry stores it.
    pub estimated_size: Option<u64>,
    pub url_info_about: Option<String>,
}

/// Whether this key is software a person would say they installed.
///
/// The reference machine has 343 keys and 317 with a name, of which 160 are
/// `SystemComponent`. What is left is roughly 157 applications. Each rule
/// here has a fixture entry where it fires and the fixture as a whole has a
/// test that the count does not drift.
pub fn is_application(e: &RawEntry) -> bool {
    if e.display_name.as_deref().unwrap_or("").trim().is_empty() {
        return false;
    }
    // Settings hides these and so does the store: runtimes, redistributables
    // and the plumbing of larger suites.
    if e.system_component.unwrap_or(0) != 0 {
        return false;
    }
    // A part of something else that has its own entry. Listing it would
    // show one application twice.
    if e.parent_key_name.is_some() {
        return false;
    }
    // Windows installed these itself and Brokey does not manage them.
    if matches!(
        e.release_type.as_deref(),
        Some("Security Update") | Some("Update") | Some("Hotfix") | Some("ServicePack")
    ) {
        return false;
    }
    true
}
```

Add `pub mod arp;` to `crates/brokey-core/src/sources/windows/mod.rs`.

Then remove the gate above the module in `crates/brokey-core/src/sources/mod.rs`,
so that `is_application` and its fixture tests are compiled on Linux too. It is a
pure function of a checked-in JSON fixture and touches no Windows API, and the
Linux runners are where most of this project's tests actually run. This is the
shape `system/windows.rs` already has: the file compiles on both platforms and
the gate sits on the items that call the Windows API, not on the tree above them.

```rust
#[cfg(unix)]
pub mod linux;
// Not gated: everything under it that calls the Windows API carries its own
// `#[cfg(windows)]`, so the pure halves stay testable on both platforms, the
// way `system::windows` does it. `linux` keeps its gate because its modules
// call unix-only APIs throughout.
pub mod windows;
```

Amend the module doc in `crates/brokey-core/src/sources/windows/mod.rs` to match:
it currently says "Built only on Windows", which stops being true here.

- [ ] **Step 5: Run the tests to verify they pass**

```powershell
cargo test -p brokey-core --lib arp
```

Expected: PASS, seven tests.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "Add/Remove Programs: the filter that decides what counts as an application

343 keys on the reference machine, 317 with a name, 160 of them
SystemComponent. The filter is a pure function over the registry values
with a fixture entry where each rule fires and a test on the total, the
way group.rs requires of its own heuristics. Driver packages fall through all
four rules and are kept, with a test guarding against a later name prefix
rule that would drop them on a machine running in another language.

sources/windows/ loses its cfg gate so the filter runs on the Linux runners
too. The gate belongs on the registry read, which is how system/windows.rs
already does it."
```

---

### Task 6: The install directory, the icon, and the package

`InstallLocation` is empty for 221 of the reference machine's 317 entries. The directory is derived from the uninstaller's own path, which is where the second spec's leftover scan will start from, so it is established and tested here.

**Files:**
- Modify: `crates/brokey-core/src/sources/windows/arp.rs`

**Interfaces:**
- Consumes: Task 5's `RawEntry`, `Hive`, `is_application`.
- Produces:
  - `arp::split_command_line(s: &str) -> Option<(String, Vec<String>)>`
  - `arp::install_dir(e: &RawEntry) -> Option<PathBuf>`
  - `arp::icon(e: &RawEntry) -> Option<Picture>`
  - `arp::package_id(e: &RawEntry) -> String`, giving `"HKLM\\7-Zip"`
  - `arp::to_package(e: &RawEntry) -> Package`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/brokey-core/src/sources/windows/arp.rs`:

```rust
    /// Windows quoting: a quoted first token can hold spaces, and what
    /// follows is split on whitespace. Getting this wrong means running
    /// "C:\Program" with an argument of "Files\...".
    #[test]
    fn a_quoted_program_with_spaces_splits_correctly() {
        let (program, args) =
            split_command_line("\"C:\\Program Files\\Obsidian\\Uninstall Obsidian.exe\" /allusers /S")
                .expect("it splits");
        assert_eq!(program, "C:\\Program Files\\Obsidian\\Uninstall Obsidian.exe");
        assert_eq!(args, ["/allusers", "/S"]);
    }

    /// The DIFX driver uninstallers are unquoted, in 8.3 short form, and
    /// take a path as an argument.
    #[test]
    fn an_unquoted_program_splits_on_whitespace() {
        let (program, args) = split_command_line(
            "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE /u C:\\WINDOWS\\System32\\a.inf",
        )
        .expect("it splits");
        assert_eq!(program, "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE");
        assert_eq!(args, ["/u", "C:\\WINDOWS\\System32\\a.inf"]);
    }

    #[test]
    fn an_empty_command_line_is_not_a_command() {
        assert_eq!(split_command_line("   "), None);
    }

    /// The registry's paths are Windows paths whatever host reads them, so the
    /// splitting is done on the text. `std::path::Path` would answer `Some("")`
    /// for the first of these off Windows, and the install directory would be
    /// silently wrong on the machine most of these tests run on.
    #[test]
    fn a_windows_parent_is_the_same_on_every_host() {
        assert_eq!(
            windows_parent("C:\\Program Files\\7-Zip\\Uninstall.exe"),
            Some("C:\\Program Files\\7-Zip".to_string())
        );
        // Installers write forward slashes too, and Windows accepts them.
        assert_eq!(
            windows_parent("C:/Program Files/7-Zip/Uninstall.exe"),
            Some("C:/Program Files/7-Zip".to_string())
        );
        // The drive's root. "C:" alone would name the drive's current
        // directory, which is somewhere else.
        assert_eq!(windows_parent("C:\\setup.exe"), Some("C:\\".to_string()));
        assert_eq!(windows_parent("setup.exe"), None);
    }

    #[test]
    fn a_windows_file_stem_is_the_same_on_every_host() {
        assert_eq!(
            windows_file_stem("C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE"),
            "DPINST~1"
        );
        assert_eq!(windows_file_stem("MsiExec.exe"), "MsiExec");
        assert_eq!(windows_file_stem("C:\\bin\\thing"), "thing");
        // A leading dot is the whole name, not an empty stem.
        assert_eq!(windows_file_stem(".gitignore"), ".gitignore");
    }

    /// The directory comes from the uninstaller, because InstallLocation is
    /// empty for 221 of the reference machine's 317 entries.
    #[test]
    fn the_install_directory_comes_from_the_uninstaller() {
        let entries = fixture();
        let obsidian = named(&entries, "Obsidian");
        assert_eq!(obsidian.install_location, None);
        assert_eq!(
            install_dir(obsidian),
            Some(std::path::PathBuf::from("C:\\Program Files\\Obsidian"))
        );
    }

    /// Where both agree, the answer is the same, which is what makes the
    /// derivation safe to rely on.
    #[test]
    fn install_location_and_the_uninstaller_agree_where_both_are_present() {
        let entries = fixture();
        assert_eq!(
            install_dir(named(&entries, "7-Zip 26.00 (x64)")),
            Some(std::path::PathBuf::from("C:\\Program Files\\7-Zip"))
        );
    }

    /// An MSI uninstaller lives in the Windows directory and says nothing
    /// about where the application is, so it is never used as the source of
    /// a directory. InstallLocation is the only answer here.
    #[test]
    fn an_msiexec_uninstaller_never_supplies_a_directory() {
        let entries = fixture();
        let per_user = named(&entries, "A per-user application");
        assert_eq!(
            install_dir(per_user),
            Some(std::path::PathBuf::from("C:\\Users\\test\\AppData\\Local\\Example"))
        );
    }

    /// DisplayIcon carries a resource index after a comma. The file is what
    /// matters; the index is dropped.
    #[test]
    fn an_icon_index_is_stripped() {
        let entries = fixture();
        let icon = icon(named(&entries, "Obsidian")).expect("there is an icon");
        assert_eq!(
            icon,
            crate::model::Picture::File(std::path::PathBuf::from(
                "C:\\Program Files\\Obsidian\\Obsidian.exe"
            ))
        );
    }

    #[test]
    fn an_entry_with_no_icon_has_none() {
        let entries = fixture();
        assert_eq!(icon(named(&entries, "A per-user application")), None);
    }

    /// The id names the hive as well as the key, because the same key name
    /// can appear in more than one of the three. The second spec addresses
    /// entries by this id.
    #[test]
    fn the_id_names_the_hive_and_the_key() {
        let entries = fixture();
        assert_eq!(package_id(named(&entries, "7-Zip 26.00 (x64)")), "HKLM\\7-Zip");
        assert_eq!(
            package_id(named(&entries, "A per-user application")),
            "HKCU\\{6f320b93-ee3c-4826-85e0-000000000002}"
        );
    }

    #[test]
    fn a_package_carries_what_the_page_draws() {
        let entries = fixture();
        let p = to_package(named(&entries, "7-Zip 26.00 (x64)"));
        assert_eq!(p.source, crate::model::SourceKind::Arp);
        assert_eq!(p.name, "7-Zip 26.00 (x64)");
        assert_eq!(p.installed_version.as_deref(), Some("26.00"));
        assert_eq!(p.version.as_deref(), Some("26.00"));
        assert!(p.installed);
        assert_eq!(p.developer.as_deref(), Some("Igor Pavlov"));
        assert_eq!(p.homepage.as_deref(), Some("https://www.7-zip.org/"));
        assert_eq!(p.kind, crate::model::PackageKind::App);
        // EstimatedSize is kilobytes in the registry and bytes in Package.
        assert_eq!(p.installed_size, Some(5133 * 1024));
    }

    /// A driver package is an application by the filter and a driver by
    /// kind, so the page can tell them apart without reading the name.
    #[test]
    fn a_driver_package_is_marked_as_a_driver() {
        let entries = fixture();
        let driver = entries
            .iter()
            .find(|e| e.display_name.as_deref().is_some_and(|n| n.contains("Arduino")))
            .expect("the fixture has one");
        assert_eq!(to_package(driver).kind, crate::model::PackageKind::Driver);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test -p brokey-core --lib arp
```

Expected: FAIL, `cannot find function split_command_line`.

- [ ] **Step 3: Implement**

Add to `crates/brokey-core/src/sources/windows/arp.rs`:

```rust
use crate::model::{Package, PackageKind, Picture, SourceKind};
use std::path::PathBuf;

/// Split a registry command line into a program and its arguments, the way
/// Windows does it: a leading quoted token may hold spaces, everything
/// after is split on whitespace. `None` when there is nothing to run.
///
/// This matters more than it looks. An uninstall string of
/// `"C:\Program Files\X\unins.exe" /S` split naively runs `C:\Program`.
pub fn split_command_line(s: &str) -> Option<(String, Vec<String>)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (program, rest) = if let Some(after) = s.strip_prefix('"') {
        let (program, rest) = after.split_once('"')?;
        (program.to_string(), rest)
    } else {
        match s.split_once(char::is_whitespace) {
            Some((program, rest)) => (program.to_string(), rest),
            None => (s.to_string(), ""),
        }
    };
    if program.is_empty() {
        return None;
    }
    let args = rest.split_whitespace().map(str::to_string).collect();
    Some((program, args))
}

/// The directory part of a Windows path, as text.
///
/// `std::path::Path` cannot do this. Off Windows a backslash is an ordinary
/// character, so `Path::new(r"C:\Program Files\X\unins.exe").parent()` answers
/// `Some("")` and every derivation below would be quietly wrong on the machine
/// most of this project's tests run on. These strings come out of a Windows
/// registry and describe a Windows machine whatever host is reading them, so
/// the splitting is spelt out and behaves the same everywhere.
fn windows_parent(path: &str) -> Option<String> {
    let cut = path.rfind(['\\', '/'])?;
    let parent = &path[..cut];
    if parent.is_empty() {
        // `\foo.exe`: the root of the current drive.
        return Some("\\".to_string());
    }
    if parent.ends_with(':') {
        // `C:\foo.exe` sits in the drive's root. `C:` alone would name the
        // drive's current directory, which is a different place.
        return Some(format!("{parent}\\"));
    }
    Some(parent.to_string())
}

/// The file name without its extension, as text, for the reason given on
/// [`windows_parent`].
fn windows_file_stem(path: &str) -> &str {
    let name = match path.rfind(['\\', '/']) {
        Some(cut) => &path[cut + 1..],
        None => path,
    };
    match name.rfind('.') {
        // A leading dot is the whole name, not an empty stem.
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// Whether a program is the Windows Installer rather than the
/// application's own uninstaller.
fn is_msiexec(program: &str) -> bool {
    windows_file_stem(program).eq_ignore_ascii_case("msiexec")
}

/// Where the application lives.
///
/// `InstallLocation` is empty for 221 of the reference machine's 317
/// entries, including every NSIS-built application on it, so the directory
/// of the uninstaller is the first answer and `InstallLocation` the
/// fallback. An `msiexec` uninstaller lives in the Windows directory and
/// says nothing about the application, so it never supplies one.
///
/// The `PathBuf` is a Windows path and is only a path on Windows.
/// Off it, take it apart with [`windows_parent`] rather than with `std::path`.
pub fn install_dir(e: &RawEntry) -> Option<PathBuf> {
    let from_uninstaller = e
        .uninstall_string
        .as_deref()
        .and_then(split_command_line)
        .filter(|(program, _)| !is_msiexec(program))
        .and_then(|(program, _)| windows_parent(&program))
        .map(PathBuf::from);
    from_uninstaller.or_else(|| {
        e.install_location
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(s.trim_end_matches(['\\', '/'])))
    })
}

/// The application's own icon. `DisplayIcon` is a path, optionally followed
/// by a comma and a resource index; the index is dropped because the page
/// asks the shell for the picture rather than for one numbered resource.
///
/// This value is never used as something to launch: it frequently points at
/// an uninstaller or at a file with no code in it at all.
pub fn icon(e: &RawEntry) -> Option<Picture> {
    let raw = e.display_icon.as_deref()?.trim();
    let raw = raw.strip_prefix('"').unwrap_or(raw);
    let path = match raw.rsplit_once(',') {
        // Only a trailing integer is an index. A bare comma in a path is not.
        Some((path, index)) if index.trim().parse::<i32>().is_ok() => path,
        _ => raw,
    };
    let path = path.trim().trim_end_matches('"');
    if path.is_empty() {
        return None;
    }
    Some(Picture::File(PathBuf::from(path)))
}

/// The id this source uses, naming the hive as well as the key, because the
/// same key name can appear in more than one of the three.
pub fn package_id(e: &RawEntry) -> String {
    format!("{}\\{}", e.hive.id(), e.key_name)
}

/// Whether the entry describes a driver rather than an application. Told by
/// the uninstaller being the Driver Install Frameworks tool, which is the
/// same on every machine, rather than by the display name, which is in the
/// language the machine was installed in.
fn is_driver(e: &RawEntry) -> bool {
    e.uninstall_string
        .as_deref()
        .and_then(split_command_line)
        .is_some_and(|(program, _)| {
            windows_file_stem(&program)
                .to_ascii_uppercase()
                .starts_with("DPINST")
        })
}

pub fn to_package(e: &RawEntry) -> Package {
    let name = e.display_name.clone().unwrap_or_default();
    let mut p = Package::new(SourceKind::Arp, package_id(e), name);
    p.kind = if is_driver(e) {
        PackageKind::Driver
    } else {
        PackageKind::App
    };
    p.installed = true;
    p.installed_version = e.display_version.clone();
    // The registry records one version and it is the installed one. Saying
    // it is also the available version keeps the Installed page from
    // drawing an update that does not exist.
    p.version = e.display_version.clone();
    p.developer = e.publisher.clone();
    p.homepage = e.url_info_about.clone();
    p.icon = icon(e);
    // EstimatedSize is kilobytes; Package counts bytes.
    p.installed_size = e.estimated_size.map(|kb| kb * 1024);
    if let Some(dir) = install_dir(e) {
        p.facts.push(("Installed to".to_string(), dir.display().to_string()));
    }
    p.facts.push(("Registry key".to_string(), package_id(e)));
    p
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test -p brokey-core --lib arp
```

Expected: PASS, twenty tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "Add/Remove Programs: derive the install directory from the uninstaller

InstallLocation is empty for 221 of the reference machine's 317 entries,
including every NSIS-built application on it, so the uninstaller's own
directory is the first answer and InstallLocation the fallback. An msiexec
uninstaller lives in the Windows directory and says nothing about the
application, so it never supplies one. DisplayIcon gives the picture and is
never used as something to launch: it often points at the uninstaller.

Windows paths are split as text rather than through std::path. Off Windows
a backslash is an ordinary character, so Path::parent on a registry path
answers an empty string and both derivations here would be wrong on the
machine most of this project tests on.

The command line splitter is the load-bearing piece. An uninstall string of
\"C:\\Program Files\\X\\unins.exe\" /S split naively runs C:\\Program."
```

---

### Task 7: The removal ladder

**Files:**
- Modify: `crates/brokey-core/src/sources/windows/arp.rs`

**Interfaces:**
- Consumes: Task 6's `split_command_line`, `install_dir`, `package_id`.
- Produces:
  - `arp::Removal` with variants `Quiet(Command)`, `Msi { product_code: String }`, `Interactive(Command)`
  - `arp::removal(e: &RawEntry) -> Option<Removal>`
  - `arp::removal_step(e: &RawEntry) -> Option<Step>`

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/brokey-core/src/sources/windows/arp.rs`:

```rust
    /// The quiet string is preferred wherever there is one: nothing opens
    /// and the plan can report honestly that it finished.
    #[test]
    fn a_quiet_uninstall_string_is_preferred() {
        let entries = fixture();
        let Some(Removal::Quiet(command)) = removal(named(&entries, "Obsidian")) else {
            panic!("Obsidian has a quiet uninstall string");
        };
        assert_eq!(
            command.program,
            "C:\\Program Files\\Obsidian\\Uninstall Obsidian.exe"
        );
        assert_eq!(command.args, ["/allusers", "/S"]);
    }

    /// 253 of the reference machine's 317 entries have no quiet string, but
    /// 204 are MSI ProductCodes and msiexec removes those silently. Only 72
    /// are left that genuinely cannot be, which is a quarter of the machine
    /// rather than most of it.
    #[test]
    fn an_msi_without_a_quiet_string_is_still_silent() {
        let entries = fixture();
        let per_user = named(&entries, "A per-user application");
        assert_eq!(per_user.quiet_uninstall_string, None);
        let Some(Removal::Msi { product_code }) = removal(per_user) else {
            panic!("an MSI ProductCode key is removable by msiexec");
        };
        assert_eq!(product_code, "{6f320b93-ee3c-4826-85e0-000000000002}");
    }

    /// What is left opens the publisher's own uninstaller, and the
    /// interface says so rather than drawing a rail that cannot move.
    ///
    /// The driver package is the entry that makes this rule load-bearing. Its
    /// key name is a well-formed GUID, so a filter that went on shape alone
    /// would call it an MSI and answer `msiexec /x` on something the Windows
    /// Installer has never heard of. It has no `WindowsInstaller` flag, which
    /// is what settles it.
    #[test]
    fn everything_else_opens_the_publishers_uninstaller() {
        let entries = fixture();
        let driver = entries
            .iter()
            .find(|e| e.display_name.as_deref().is_some_and(|n| n.contains("Arduino")))
            .expect("the fixture has one");
        let Some(Removal::Interactive(command)) = removal(driver) else {
            panic!("a DIFX driver has no quiet string and is not a Windows Installer product");
        };
        assert_eq!(command.program, "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE");
    }

    #[test]
    fn an_entry_with_no_uninstaller_at_all_cannot_be_removed() {
        let entries = fixture();
        assert!(removal(named(&entries, "Update for Windows")).is_none());
    }

    /// A key name that merely looks a bit like a GUID is not one. Only the
    /// exact ProductCode shape is treated as an MSI.
    #[test]
    fn a_key_name_that_is_not_a_product_code_is_not_an_msi() {
        let entries = fixture();
        assert!(matches!(
            removal(named(&entries, "7-Zip 26.00 (x64)")),
            Some(Removal::Quiet(_))
        ));
    }

    /// Removing what the whole machine has needs Administrator; removing
    /// what only this user has does not. This is what keeps the common
    /// case free of a prompt.
    #[test]
    fn only_a_machine_wide_entry_needs_elevation() {
        let entries = fixture();
        let machine = removal_step(named(&entries, "Obsidian")).expect("a step");
        let user = removal_step(named(&entries, "A per-user application")).expect("a step");
        assert!(machine.needs_root);
        assert!(!user.needs_root);
        assert_eq!(machine.source, crate::model::SourceKind::Arp);
        assert_eq!(machine.title, "Removing Obsidian");
    }

    #[test]
    fn an_msi_step_runs_msiexec_quietly() {
        let entries = fixture();
        let step = removal_step(named(&entries, "A per-user application")).expect("a step");
        assert_eq!(step.command.program, "msiexec.exe");
        assert_eq!(
            step.command.args,
            ["/x", "{6f320b93-ee3c-4826-85e0-000000000002}", "/qn", "/norestart"]
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test -p brokey-core --lib arp
```

Expected: FAIL, `cannot find type Removal`.

- [ ] **Step 3: Implement**

Add to `crates/brokey-core/src/sources/windows/arp.rs`:

```rust
use crate::model::{Command, Step};

/// How an entry comes off the machine, best route first.
///
/// The two registry values have to be read together to get the real
/// picture. On the reference machine 253 of 317 entries have no
/// `QuietUninstallString` and 204 are MSI ProductCodes, but only 23 are
/// both: 245 can be removed silently and 72 cannot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Removal {
    /// The publisher gave a silent switch. Nothing opens.
    Quiet(Command),
    /// The uninstall key carries `WindowsInstaller = 1` and is named by a
    /// well-formed ProductCode, so the Windows Installer removes it silently
    /// whatever the uninstall string says.
    Msi { product_code: String },
    /// The publisher's own uninstaller opens a window the user clicks
    /// through. The interface says so rather than drawing a progress rail.
    Interactive(Command),
}

/// Whether a key name is shaped like an MSI ProductCode: braces around a
/// GUID. The shape alone proves nothing, because plenty of things are keyed
/// by a GUID without being MSI products; `removal` asks for the
/// `WindowsInstaller` flag as well.
fn product_code(key_name: &str) -> Option<&str> {
    let inner = key_name.strip_prefix('{')?.strip_suffix('}')?;
    let groups: Vec<&str> = inner.split('-').collect();
    let expected = [8, 4, 4, 4, 12];
    if groups.len() != expected.len() {
        return None;
    }
    for (group, length) in groups.iter().zip(expected) {
        if group.len() != length || !group.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(key_name)
}

fn command(line: &str) -> Option<Command> {
    let (program, args) = split_command_line(line)?;
    Some(Command {
        program,
        args,
        env: Vec::new(),
        cwd: None,
    })
}

pub fn removal(e: &RawEntry) -> Option<Removal> {
    if let Some(quiet) = e.quiet_uninstall_string.as_deref().and_then(command) {
        return Some(Removal::Quiet(quiet));
    }
    // The flag says the Windows Installer owns this product; the shape says
    // the key name is safe to hand it as an argument. A DIFX driver package
    // is keyed by a GUID and has the shape without the flag, and msiexec
    // would fail on it while Brokey reported a silent removal.
    if e.windows_installer == Some(1)
        && let Some(code) = product_code(&e.key_name)
    {
        return Some(Removal::Msi {
            product_code: code.to_string(),
        });
    }
    let interactive = e.uninstall_string.as_deref().and_then(command)?;
    Some(Removal::Interactive(interactive))
}

/// The step that removes the entry. `needs_root` means Administrator here:
/// software the whole machine has needs it, software only this user has
/// does not, which is what keeps the common case free of a prompt.
pub fn removal_step(e: &RawEntry) -> Option<Step> {
    let name = e.display_name.clone().unwrap_or_else(|| e.key_name.clone());
    let command = match removal(e)? {
        Removal::Quiet(c) | Removal::Interactive(c) => c,
        Removal::Msi { product_code } => Command {
            program: "msiexec.exe".to_string(),
            args: vec![
                "/x".to_string(),
                product_code,
                "/qn".to_string(),
                "/norestart".to_string(),
            ],
            env: Vec::new(),
            cwd: None,
        },
    };
    Some(Step {
        source: SourceKind::Arp,
        title: format!("Removing {name}"),
        command,
        needs_root: e.hive.needs_elevation(),
        weight: 10,
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test -p brokey-core --lib arp
```

Expected: PASS, twenty-seven tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "Add/Remove Programs: the three routes off the machine

The quiet string where there is one, msiexec where the key is a ProductCode,
and the publisher's own uninstaller otherwise. The two registry values have
to be read together: 253 of the reference machine's 317 entries have no
quiet string and 204 are ProductCodes, but only 23 are both, so 245 can be
removed silently and 72 cannot.

Machine-wide entries need Administrator and per-user ones do not, which is
what keeps the common case free of a prompt."
```

---

### Task 8: Read the registry, implement `Source`, and list what this machine has

**Files:**
- Modify: `crates/brokey-core/src/sources/windows/arp.rs`
- Modify: `crates/brokey-core/src/sources/mod.rs`
- Modify: `.github/workflows/ci.yml:96`
- Modify: `README.md`

**Interfaces:**
- Consumes: everything from Tasks 5 to 7.
- Produces: `arp::Arp` implementing `Source`, and `arp::read() -> Vec<RawEntry>`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/brokey-core/src/sources/windows/arp.rs`:

```rust
    use crate::{Op, Query, Source};

    fn source_from_fixture() -> Arp {
        Arp {
            entries: fixture(),
        }
    }

    /// The registry has no notion of a newer version, so this source never
    /// searches and never reports an update. A source that answered either
    /// would be inventing something.
    #[test]
    fn it_neither_searches_nor_updates() {
        let arp = source_from_fixture();
        assert!(arp.search(&Query::new("obsidian")).unwrap().is_empty());
        assert!(arp.updates().unwrap().is_empty());
    }

    #[test]
    fn installed_is_the_filtered_entries_as_packages() {
        let arp = source_from_fixture();
        let installed = arp.installed().unwrap();
        assert_eq!(installed.len(), 5);
        assert!(installed.iter().all(|p| p.installed));
        assert!(
            installed
                .iter()
                .any(|p| p.name == "Obsidian" && p.id == "HKLM\\Obsidian")
        );
    }

    #[test]
    fn details_answers_for_an_id_it_has_and_says_so_for_one_it_does_not() {
        let arp = source_from_fixture();
        assert_eq!(arp.details("HKLM\\Obsidian").unwrap().name, "Obsidian");
        let e = arp.details("HKLM\\Nothing").unwrap_err();
        assert!(e.message.contains("HKLM\\Nothing"), "{}", e.message);
    }

    /// The only operation this source plans. Anything else is a source that
    /// has nothing to do, which is an empty list rather than an error.
    #[test]
    fn it_plans_a_removal_and_nothing_else() {
        let arp = source_from_fixture();
        let reference = crate::model::PackageRef {
            source: SourceKind::Arp,
            id: "HKLM\\Obsidian".to_string(),
        };
        let steps = arp
            .plan(&Op::Remove {
                package: reference.clone(),
            })
            .unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].title, "Removing Obsidian");

        let install = arp.plan(&Op::Install { package: reference }).unwrap();
        assert!(install.is_empty());
    }

    /// The source is always there: the registry is part of Windows. It says
    /// how many applications it found, for the status bar.
    #[test]
    fn it_is_always_available_and_says_how_much_it_found() {
        let arp = source_from_fixture();
        let status = arp.status();
        assert!(status.available);
        assert_eq!(status.reason, None);
        assert_eq!(status.detail.as_deref(), Some("5 applications"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test -p brokey-core --lib arp
```

Expected: FAIL, `cannot find struct Arp`.

- [ ] **Step 3: Implement the source**

Add to `crates/brokey-core/src/sources/windows/arp.rs`:

```rust
use crate::model::Op;
use crate::{Error, Query, Result, Source, SourceStatus, Update};

/// The Add/Remove Programs source. The entries are read once when the
/// source is built, as the pacman source reads its database once.
pub struct Arp {
    pub(crate) entries: Vec<RawEntry>,
}

impl Arp {
    /// Gated because it reads the registry. The `Source` implementation
    /// below is not: it answers from `entries`, so the fixture tests
    /// compile and run on Linux as well.
    #[cfg(windows)]
    pub fn new() -> Arp {
        Arp { entries: read() }
    }

    fn applications(&self) -> impl Iterator<Item = &RawEntry> {
        self.entries.iter().filter(|e| is_application(e))
    }

    fn find(&self, id: &str) -> Option<&RawEntry> {
        self.applications().find(|e| package_id(e) == id)
    }
}

#[cfg(windows)]
impl Default for Arp {
    fn default() -> Arp {
        Arp::new()
    }
}

impl Source for Arp {
    fn kind(&self) -> SourceKind {
        SourceKind::Arp
    }

    fn status(&self) -> SourceStatus {
        let found = self.applications().count();
        SourceStatus {
            kind: SourceKind::Arp,
            available: true,
            reason: None,
            detail: Some(format!("{found} applications")),
            searchable: false,
            setup: None,
        }
    }

    /// The registry is a record of what is here, not a catalogue of what
    /// could be. Searching it would return only what is already installed,
    /// which the Installed page already shows.
    fn search(&self, _query: &Query) -> Result<Vec<crate::Package>> {
        Ok(Vec::new())
    }

    fn installed(&self) -> Result<Vec<crate::Package>> {
        Ok(self.applications().map(to_package).collect())
    }

    /// An uninstall key records one version, the installed one, and knows
    /// nothing about a newer one.
    fn updates(&self) -> Result<Vec<Update>> {
        Ok(Vec::new())
    }

    fn details(&self, id: &str) -> Result<crate::Package> {
        self.find(id).map(to_package).ok_or_else(|| {
            Error::from_source(
                SourceKind::Arp,
                format!("{id} is not in the uninstall registry. It may have been removed already."),
            )
        })
    }

    fn plan(&self, op: &Op) -> Result<Vec<Step>> {
        let Op::Remove { package } = op else {
            return Ok(Vec::new());
        };
        let entry = self.find(&package.id).ok_or_else(|| {
            Error::from_source(
                SourceKind::Arp,
                format!(
                    "{} is not in the uninstall registry, so there is nothing to remove.",
                    package.id
                ),
            )
        })?;
        Ok(removal_step(entry).into_iter().collect())
    }
}

/// Read the three uninstall keys. The only impure function in the module,
/// and the only one gated: `windows-registry` is a Windows-only dependency,
/// so everything else here stays compiled and tested on both platforms.
#[cfg(windows)]
pub fn read() -> Vec<RawEntry> {
    let mut entries = Vec::new();
    let roots: [(Hive, &windows_registry::Key, &str); 3] = [
        (
            Hive::Machine,
            &windows_registry::LOCAL_MACHINE,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            Hive::Machine32,
            &windows_registry::LOCAL_MACHINE,
            r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
        (
            Hive::User,
            &windows_registry::CURRENT_USER,
            r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        ),
    ];
    for (hive, root, path) in roots {
        let Ok(key) = root.open(path) else { continue };
        let Ok(names) = key.keys() else { continue };
        for name in names {
            let Ok(sub) = key.open(&name) else { continue };
            entries.push(RawEntry {
                hive,
                key_name: name,
                display_name: sub.get_string("DisplayName").ok(),
                display_version: sub.get_string("DisplayVersion").ok(),
                publisher: sub.get_string("Publisher").ok(),
                install_location: sub.get_string("InstallLocation").ok(),
                uninstall_string: sub.get_string("UninstallString").ok(),
                quiet_uninstall_string: sub.get_string("QuietUninstallString").ok(),
                windows_installer: sub.get_u32("WindowsInstaller").ok(),
                display_icon: sub.get_string("DisplayIcon").ok(),
                system_component: sub.get_u32("SystemComponent").ok(),
                parent_key_name: sub.get_string("ParentKeyName").ok(),
                release_type: sub.get_string("ReleaseType").ok(),
                estimated_size: sub.get_u32("EstimatedSize").ok().map(u64::from),
                url_info_about: sub.get_string("URLInfoAbout").ok(),
            });
        }
    }
    entries
}
```

Leave `RawEntry`'s derives as Task 5 wrote them. The fixture spells every field and `read` fills every field, so there is nothing for `#[serde(default)]` to do except turn a field missing from the fixture into a silent `None` instead of a parse error.

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test -p brokey-core --lib arp
```

Expected: PASS, thirty-two tests, with the live test ignored.

- [ ] **Step 5: Wire it into the source list**

In `crates/brokey-core/src/sources/mod.rs`, replace the Windows arm of `all`:

```rust
    #[cfg(windows)]
    {
        let _ = (system, client, catalogue, preferences);
        vec![Box::new(windows::arp::Arp::new())]
    }
```

Change the Task 1 test `windows_has_no_sources_yet` to assert the list holds exactly `SourceKind::Arp`, and rename it `windows_has_the_add_remove_programs_source`.

- [ ] **Step 6: Add a live test against this machine**

Add to `crates/brokey-core/src/sources/windows/arp.rs`, in the `tests` module:

```rust
    /// The real registry on the machine running the tests. Ignored by
    /// default because its answer depends on what is installed, and run
    /// with `cargo test -- --ignored live_arp` when the parser changes.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn live_arp_reads_this_machine() {
        let arp = Arp::new();
        let installed = arp.installed().unwrap();
        assert!(
            installed.len() > 10,
            "a real Windows machine has more than ten applications, found {}",
            installed.len()
        );
        assert!(installed.iter().all(|p| !p.name.is_empty()));
        // Every one of them must be removable somehow, or the ladder has a
        // gap that the fixture did not show.
        let stuck: Vec<&str> = arp
            .applications()
            .filter(|e| removal(e).is_none())
            .filter_map(|e| e.display_name.as_deref())
            .collect();
        assert!(stuck.is_empty(), "no route off the machine for {stuck:?}");
    }
```

- [ ] **Step 7: Run it and read the output**

```powershell
cargo test -p brokey-core --lib -- --ignored live_arp --nocapture
cargo run -p brokey -- sources
cargo run -p brokey -- installed
```

Expected: the live test passes; `sources` names `Installed` as available with a count; `installed` prints the real applications on this machine. If `live_arp` names entries with no route off the machine, that is a real gap in Task 7's ladder: fix it there, with a fixture entry, rather than loosening the assertion.

- [ ] **Step 8: Add the Windows CI job**

In `.github/workflows/ci.yml`, add `windows-latest` to the matrix at line 96:

```yaml
        os: [ubuntu-22.04, ubuntu-22.04-arm, windows-latest]
```

and guard the Linux-only step so the Windows runner skips it:

```yaml
      - name: Install Linux dependencies
        if: runner.os == 'Linux'
        run: |
```

- [ ] **Step 9: Update the honest lists**

In `README.md`, under "What is not there yet", add:

```markdown
- Windows support is being built and is not in a release yet. On Windows, Brokey lists what Add/Remove Programs knows about and works out how each entry would be removed. It cannot install anything there, and removal is not wired up either: the helper is a stub on Windows, so the plan is built and nothing runs it. winget, Chocolatey, Scoop and the Microsoft Store come next.
```

Leave `CHANGELOG.md` alone. The current version is 0.1.4, `v0.1.4` is tagged, and that section is already written, so `crates/brokey/tests/release.rs` passes untouched. Editing it would rewrite notes that have been published, which the changelog's own header forbids. Writing a new section instead would mean bumping the version in `Cargo.toml`, `tauri.conf.json` and `package.json`, which `release.rs` checks together, and cutting a release is not this plan's to do. The release that ships Windows writes its own notes.

- [ ] **Step 10: Run every check on both platforms**

```powershell
cargo fmt --all --check; if ($?) { cargo clippy --workspace --all-targets }; if ($?) { cargo test --workspace }
```

Do not push. This branch has not been pushed, and doing so is the controller's call rather than this task's. It would also tell you nothing: this repository's CI runs on a push to `main`, on a pull request, or on a manual dispatch, so a plain branch push starts no jobs. Report the Linux and ARM runners as unverified from here, and say what you reasoned about them instead of running them.

- [ ] **Step 11: Commit**

```bash
git add -A
git commit -m "Brokey lists what a Windows machine has installed

The Add/Remove Programs source reads the three uninstall keys and answers
installed and plan(Remove). It never searches and never reports an update,
because the registry records one version and knows nothing about a newer
one; a source that answered either would be inventing something.

CI gains a windows-latest runner. README says plainly that installing on
Windows does not work yet."
```

---

## What this plan does not build

Named here so no one goes looking. Each is in plan 2 or plan 3:

- Every source except Add/Remove Programs: winget, Chocolatey, Scoop, Store and MSIX, Windows features, and the GitHub source's Windows assets.
- The elevated helper, the named pipe, and the Windows closed list. Task 1 leaves `brokey-helper` a stub on Windows and Task 7's `needs_root` is recorded but nothing acts on it yet, so a machine-wide removal is planned correctly and cannot yet be run.
- `Source::setup` for any Windows source.
- `launch.rs` on Windows, so the Installed page has no Open button there.
- The metadata ladder, including the extracted `.exe` icon: Task 6 records the icon's path in `Picture::File` and the page's existing asset protocol draws it, but nothing extracts an icon from a resource index yet.
- Self-update detection, packaging, and the page's nav and status bar changes.
