# Leftover cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After Brokey removes a Windows application, find what its uninstaller left behind, guard out anything another application uses, and let the user move the rest to a restorable holding area; plus forced removal, Steam games through Steam, and self-elevating uninstallers.

**Architecture:** A new `brokey-core/src/cleanup/` module: hints taken at plan time, a pure scanner over a read-only `SystemView` trait, an ownership guard, and a holding area. Clearing, restoring and emptying are new `Op`s the planner expands into steps whose `Command.program` is a reserved internal name (`brokey:hold` and friends); the Runner executes those in-process for session steps and the helper executes them for root steps, after `allow.rs` validates their structured payload. The page learns a scan is ready through a new `Event::Leftovers` and fetches it by id.

**Tech Stack:** Rust 2024 (brokey-core, brokey-helper, brokey), `windows-registry` / `windows-sys` / `windows` crates already in the workspace, Tauri 2, React 19 + TypeScript, plain CSS in `app.css`.

**Spec:** `docs/superpowers/specs/2026-11-24-leftover-cleanup-design.md`

## Global Constraints

- Every rule in `CLAUDE.md` holds; read it first. In particular: a source never runs anything; the window never elevates; a privileged operation is an entry on the closed list with a test that it refuses anything else; progress is honest (`fraction: None` plus a sentence while waiting); colour is state, so confidence is a word in a neutral `Badge`; never a raw hex; §7 components only.
- Copy: British spelling, sentence case, full stops in sentences, no em dashes, no emoji, errors say what went wrong and what to do.
- Every new type the page receives gets a sample in `crates/brokey/tests/contract.rs` and a declaration in `frontend/src/types.ts`.
- Every heuristic arrives with a fixture where it fires and one where it must not.
- Confidence words, exactly: `Certain`, `Likely`, `Possible`, `Your data`. Certain and Likely ticked by default; Possible and Your data unticked.
- Name matching: the winget normaliser (`sources/windows/winget/normalise.rs`); never match a normalised name shorter than 4 characters; stop list exactly: `common`, `shared`, `data`, `update`, `temp`, `microsoft`, `windows`, `intel`, `nvidia`, `amd`, `google`.
- Holding area: user items `%LOCALAPPDATA%\Brokey\Holding\<id>\`, machine items `%ProgramData%\Brokey\Holding\<id>\` (Administrators-only ACL). Auto-empty after 30 days; never an Administrator prompt at start-up.
- The helper refuses only name-surrogate reparse points (junction, symlink, mount point) anywhere on a path; cloud-synced items are excluded by the scanner, not the helper.
- The self-elevating uninstaller path is `ShellExecuteExW` with verb `open` (never `runas`), lives in `transaction/elevate/windows.rs`, and is reached only for a step the closed list refused with `can_be_replaced`.
- Version: this ships in 0.1.8 (already the workspace version); add bullets to the existing `## 0.1.8` section of `CHANGELOG.md`.
- Gates, all from the repo root: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets` (no warnings), `cargo test --workspace`, `npm run lint`, `npm run check:design`, `sh packaging/check.sh`. A fresh worktree needs `npm ci && npm run build` before `cargo build` (for `frontend/dist`).

## Review Focus

1. **Two applications from one publisher.** Removing one must not offer `Software\<Publisher>`, `%APPDATA%\<Publisher>` or `Program Files\<Publisher>` while the other is installed. Pinned in Task 5.
2. **An install directory that is a broad folder** (`InstallLocation` = `C:\Program Files`, a drive root, the user's Downloads or Desktop). The folder itself must never be offered. Pinned in Task 4.
3. **The user cancels the publisher's uninstaller.** The entry stays; no leftovers are offered for a still-installed application, and forced removal is offered instead. Pinned in Task 8.
4. **A file is locked during the move** (the application is still running). The items already moved stay recorded in the manifest and restorable, the failed ones are named, nothing is lost. Pinned in Task 6.
5. **A folder swapped for a junction between scan and clear.** The helper refuses the whole step before touching anything. Pinned in Task 7.

---

### Task 1: Types, ops and the event

**Files:**
- Create: `crates/brokey-core/src/cleanup/mod.rs` (module root, re-exports)
- Modify: `crates/brokey-core/src/lib.rs` (add `pub mod cleanup;`), `crates/brokey-core/src/model.rs`
- Modify: `crates/brokey/tests/contract.rs`, `frontend/src/types.ts`

**Interfaces:**
- Produces, in `model.rs`, all `Serialize, Deserialize, Clone, Debug, PartialEq`, serde style matching the file (`rename_all = "lowercase"` / tagged like `Op`):
  - `enum Confidence { Certain, Likely, Possible, YourData }` serialised `"certain" | "likely" | "possible" | "yourdata"`.
  - `enum ItemKind { File, Folder, RegistryKey, RegistryValue, Shortcut, Service, Task, FirewallRule }`.
  - `enum Scope { User, Machine }`.
  - `struct LeftoverItem { id: String, kind: ItemKind, location: String, size: Option<u64>, confidence: Confidence, scope: Scope, ticked: bool, reason: String }` (`reason`: one sentence why it matched, shown on hover).
  - `struct Findings { scan: String, package: PackageRef, name: String, items: Vec<LeftoverItem>, kept: Vec<KeptItem>, not_offered_cloud: u32, unreadable: u32 }` and `struct KeptItem { location: String, used_by: String }`.
  - `struct HoldingEntry { id: String, name: String, created: String /* RFC 3339 */, size: u64, items: u32, scope_machine: bool, waiting: bool /* machine items past 30 days not yet emptied */ }`.
  - New `Op` variants: `ClearLeftovers { scan: String, items: Vec<String> }`, `Restore { holding: String }`, `EmptyHolding { holding: Option<String> }` (None = all), `ForceRemove { package: PackageRef }`.
  - New `Event` variants: `Leftovers { plan: String, scan: String, count: usize }` and `Waiting { plan: String, step: usize, message: String }` (the "waiting for the uninstaller" state, so the page can show its two buttons).
- `cleanup/mod.rs` declares submodules `hints`, `view`, `scan`, `guard`, `holding`, `internal` (empty files with module docs are fine in this task).

- [ ] **Step 1:** Add contract samples in `contract.rs` for `Findings` (one item of each `Confidence`, one `KeptItem`), `HoldingEntry`, each new `Op` and each new `Event`, following the file's existing sample pattern.
- [ ] **Step 2:** Run `cargo test -p brokey --test contract`. Expected: FAIL (types missing in Rust and/or `types.ts`).
- [ ] **Step 3:** Add the Rust types and the matching `types.ts` declarations; make every exhaustive `match` on `Op` / `Event` compile (planner arms may return `Err("not yet")` until Task 8; `summary()` in runner gets real wording now: "Cleared leftovers from …", "Restored …", "Emptied the holding area", "Forced removal of …").
- [ ] **Step 4:** Run `cargo test --workspace` and `npm run lint`. Expected: PASS.
- [ ] **Step 5:** Commit: `Cleanup: the types, ops and events the page and the planner share`.

### Task 2: The read-only system view

**Files:**
- Create: `crates/brokey-core/src/cleanup/view.rs`, `crates/brokey-core/src/cleanup/view/windows.rs` (cfg(windows)), `crates/brokey-core/src/cleanup/view/fixture.rs`
- Test fixtures: `crates/brokey-core/tests/fixtures/cleanup/` (JSON machine snapshots)

**Interfaces:**
- Produces:
  ```rust
  pub enum Reparse { None, NameSurrogate, Cloud }
  pub struct Entry { pub name: String, pub is_dir: bool, pub size: u64, pub reparse: Reparse }
  pub enum Hive { Hklm, Hklm32, Hkcu }
  pub struct Service { pub name: String, pub image_path: String }
  pub struct ScheduledTask { pub path: String, pub actions: Vec<String> }
  pub struct FirewallRule { pub name: String, pub program: String }
  pub trait SystemView {
      fn known_folder(&self, which: Known) -> Option<PathBuf>; // Known: ProgramFiles, ProgramFilesX86, ProgramData, AppData, LocalAppData, LocalLow, Documents, SavedGames, StartMenuMachine, StartMenuUser, Desktop, Windows, Profile
      fn list(&self, dir: &Path) -> Result<Vec<Entry>, ViewError>;      // ViewError::Unreadable counts toward Findings.unreadable
      fn exists(&self, path: &Path) -> bool;
      fn reparse(&self, path: &Path) -> Reparse;                          // of the path itself
      fn dir_size(&self, dir: &Path) -> u64;
      fn subkeys(&self, hive: Hive, path: &str) -> Result<Vec<String>, ViewError>;
      fn values(&self, hive: Hive, path: &str) -> Result<Vec<(String, String)>, ViewError>; // value data as text
      fn shortcut_target(&self, lnk: &Path) -> Option<PathBuf>;           // via sources/windows/shortcut.rs
      fn services(&self) -> Vec<Service>;
      fn tasks(&self) -> Vec<ScheduledTask>;
      fn firewall_rules(&self) -> Vec<FirewallRule>;
  }
  pub struct FixtureView; // FixtureView::load(path: &Path) -> FixtureView, a JSON tree of the same shapes
  #[cfg(windows)] pub struct LiveView; // LiveView::new()
  ```
- `LiveView`: files via `std::fs` + `GetFileAttributesW`/`FILE_ATTRIBUTE_REPARSE_POINT` and the reparse tag (`IO_REPARSE_TAG_MOUNT_POINT`, `IO_REPARSE_TAG_SYMLINK` → NameSurrogate; `IO_REPARSE_TAG_CLOUD*` or `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`/`RECALL_ON_OPEN` → Cloud); registry via `windows-registry`; services from `HKLM\SYSTEM\CurrentControlSet\Services\*\ImagePath`; tasks from `C:\Windows\System32\Tasks` XML files' `<Exec><Command>` (readable ones only; unreadable count); firewall from `HKLM\SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules` values' `App=` field. Nothing shells out.

- [ ] **Step 1:** Test `fixture_view_answers_what_the_json_says` in `view/fixture.rs`: load `tests/fixtures/cleanup/minimal.json`, assert `list`, `subkeys`, `values`, `services`, `reparse` return the fixture's entries, and that a path marked unreadable returns `Err(ViewError::Unreadable)`.
- [ ] **Step 2:** Run `cargo test -p brokey-core cleanup::view`. Expected: FAIL.
- [ ] **Step 3:** Implement the trait, `FixtureView`, and `LiveView`.
- [ ] **Step 4:** Add ignored `live_view_reads_this_machine` (cfg(windows)) asserting `known_folder(ProgramFiles)` exists, `services()` is non-empty, and `reparse` of `%USERPROFILE%\OneDrive` (when present) is `Cloud` or `None`, never panics. Run it with `-- --ignored live_view`. Expected: PASS on the reference machine.
- [ ] **Step 5:** Run `cargo test -p brokey-core cleanup::view`; commit `Cleanup: a read-only view of the machine, live and from fixtures`.

### Task 3: Hints

**Files:**
- Create: `crates/brokey-core/src/cleanup/hints.rs`
- Modify: `crates/brokey-core/src/lib.rs` (`Store::leftover_hints`)

**Interfaces:**
- Produces:
  ```rust
  pub struct Hints {
      pub package: PackageRef, pub name: String, pub publisher: Option<String>,
      pub uninstall_key: Option<(Hive, String)>, pub product_code: Option<String>,
      pub install_dir: Option<PathBuf>, pub install_location: Option<PathBuf>,
      pub icon_dir: Option<PathBuf>, pub shortcuts: Vec<PathBuf>,
      pub extra_dirs: Vec<PathBuf>, // scoop apps\<name>, choco lib\<id>, github portable folder
  } // Serialize, Deserialize, Clone, Debug, PartialEq
  pub fn from_arp(entry: &arp::RawEntry, shortcuts: Vec<PathBuf>) -> Hints;
  impl Store { pub fn leftover_hints(&self, package: &PackageRef) -> Option<Hints> }
  ```
- `leftover_hints`: for `arp` use the entry; for winget/choco/scoop use the Add/Remove entry named by the package's `arp::REGISTRY_KEY_FACT` fact plus the source's own folder in `extra_dirs`; for github use `InstallRecord.path`; `msix` returns hints with no key and no install dir (scan of general locations only); Linux sources return `None` for now.

- [ ] **Step 1:** Tests in `hints.rs`: `from_arp_takes_the_derived_install_dir_and_product_code` (an MSI-named fixture entry from `tests/fixtures/windows/arp/entries.json`), `from_arp_on_an_unquoted_steam_path_finds_the_steam_folder`, `a_winget_package_takes_its_joined_entry` (Store built from fixtures as existing winget tests do).
- [ ] **Step 2:** Run; expected FAIL. **Step 3:** Implement. **Step 4:** Run; expected PASS.
- [ ] **Step 5:** Commit `Cleanup: the hints a scan searches by, taken before the uninstaller runs`.

### Task 4: The scanner

**Files:**
- Create: `crates/brokey-core/src/cleanup/scan.rs`
- Fixtures: `tests/fixtures/cleanup/discord.json`, `steam-game.json`, `broad-install-dir.json`, `onedrive-documents.json`, `short-name.json`

**Interfaces:**
- Consumes: `Hints` (Task 3), `SystemView` (Task 2), `winget::normalise` (existing).
- Produces:
  ```rust
  pub struct Candidate { pub kind: ItemKind, pub location: String, pub size: Option<u64>, pub confidence: Confidence, pub scope: Scope, pub reason: String }
  pub struct Scanned { pub candidates: Vec<Candidate>, pub not_offered_cloud: u32, pub unreadable: u32 }
  pub fn scan(hints: &Hints, view: &dyn SystemView) -> Scanned;
  ```
- Locations and rules exactly as spec §3 and §4. Registry locations are written `HKLM\…`, `HKLM32\…`, `HKCU\…` in `location`; values as `<key> | <value name>`.
- A path pointing into `install_dir` is `Certain`; install dir itself `Certain` **only if** it passes `is_specific(dir)` (reuse `sources/windows/shortcut.rs`'s `specific`, which already refuses drive roots, Program Files, ProgramData, profile and AppData roots; extend it with Downloads, Desktop, Documents); name matches `Likely`; publisher-only `Possible`; anything under Documents / Saved Games / My Games `YourData`.
- Cloud-reparse entries and anything beneath them: counted in `not_offered_cloud`, never a candidate. Name-surrogate entries: never a candidate, never descended.

- [ ] **Step 1:** Tests: `discord_leftovers_are_found_with_the_right_confidence` (AppData\Local\Discord Certain via Update.exe in install dir, Roaming\discord Likely, HKCU\Software\Discord Likely, Run value Certain, Start menu folder Certain); `a_steam_game_offers_its_common_folder_and_my_games_as_your_data`; `a_broad_install_dir_is_never_offered` (InstallLocation `C:\Program Files`, and one of `C:\Users\you\Downloads`); `onedrive_documents_are_counted_not_offered` (assert `not_offered_cloud == 2`, no candidate under Documents); `a_three_letter_name_matches_nothing` (app named `Foo` with `%APPDATA%\Foo` present: no candidate by name); `stop_list_words_never_match` (publisher `Microsoft`: no `Software\Microsoft` candidate); `a_junction_inside_appdata_is_not_followed`.
- [ ] **Step 2:** Run `cargo test -p brokey-core cleanup::scan`; expected FAIL. **Step 3:** Implement `scan`. **Step 4:** expected PASS.
- [ ] **Step 5:** Commit `Cleanup: the scanner, its locations and its confidence`.

### Task 5: The ownership guard and protected locations

**Files:**
- Create: `crates/brokey-core/src/cleanup/guard.rs`

**Interfaces:**
- Consumes: `Candidate` (Task 4), `SystemView` (Task 2), `arp::read`, `arp::install_dir`.
- Produces:
  ```rust
  pub struct Claims { /* paths and registry keys, each with the claiming application's display name */ }
  pub fn claims(view: &dyn SystemView, installed: &[Package], excluding: Option<&Hints>) -> Claims;
  pub fn is_protected(location: &str, view: &dyn SystemView) -> bool;
  pub fn guard(candidates: Vec<Candidate>, claims: &Claims, view: &dyn SystemView) -> (Vec<Candidate>, Vec<KeptItem>);
  pub fn findings(scan_id: String, hints: &Hints, scanned: Scanned, claims: &Claims, view: &dyn SystemView) -> Findings; // assigns item ids, ticked per Confidence
  ```
- Claims, exactly spec §5: other Add/Remove entries' derived install dirs and `InstallLocation`; Windows Installer components under `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Installer\UserData\*\Components\*` (values are product → path; a component claims its path for every product except `excluding`'s packed product code); `%LOCALAPPDATA%\Packages\*`; other Scoop `apps\*` and Chocolatey `lib\*`; other apps' Start menu shortcut targets; services/tasks whose program is outside the install dir.
- A candidate is dropped (to `KeptItem`) if it **is, contains, or lies inside** a claim. A publisher-level candidate (`Software\<Publisher>`, `%APPDATA%\<Publisher>`, `Program Files\<Publisher>`) is dropped while any other installed package has that publisher.
- Protected, never offered, not even as kept: Windows dir; roots of Program Files (both), ProgramData, AppData, LocalAppData, LocalLow, profile, Start menu (both); `Software\Microsoft` and below except an app's own `Uninstall\<key>` and `App Paths\<exe>`, `Run` values; `Software\Classes\.<ext>` keys referenced by any other ProgID; `Software\Policies`; `Software\WOW6432Node` itself.

- [ ] **Step 1:** Tests: `two_apps_from_one_publisher_keep_the_publisher_folder_and_key` (Review Focus 1); `an_msi_component_shared_with_another_product_is_kept`; `the_product_being_force_removed_does_not_shield_its_own_files` (`excluding` = its hints); `kept_items_name_who_uses_them`; `protected_locations_are_never_offered` (one assertion per protected root); `findings_tick_certain_and_likely_only`.
- [ ] **Step 2:** Run; FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5:** Commit `Cleanup: nothing another application uses is offered, and protected places never are`.

### Task 6: The holding area

**Files:**
- Create: `crates/brokey-core/src/cleanup/holding.rs`, `crates/brokey-core/src/cleanup/holding/windows.rs` (cfg(windows) mutating backends)

**Interfaces:**
- Produces:
  ```rust
  pub struct HoldItem { pub kind: ItemKind, pub location: String }      // Serialize, Deserialize
  pub struct Manifest { pub id: String, pub name: String, pub created: String, pub items: Vec<Held> } // Held = HoldItem + where it went + exported file
  pub trait Mutate { /* move_path, export_key, delete_key, export_service, delete_service, export_task, delete_task, delete_firewall_rule, and their restores */ }
  pub fn hold(root: &Path, id: &str, name: &str, items: &[HoldItem], m: &mut dyn Mutate) -> HoldReport; // HoldReport { held: usize, failed: Vec<(String, String /* sentence */)> }
  pub fn restore(root: &Path, id: &str, m: &mut dyn Mutate) -> HoldReport; // refuses item by item where the place is now occupied
  pub fn empty(root: &Path, id: Option<&str>, older_than_days: Option<u32>) -> HoldReport;
  pub fn list(user_root: &Path, machine_root: &Path) -> Vec<HoldingEntry>; // waiting = machine entry older than 30 days
  pub fn user_root() -> PathBuf; pub fn machine_root() -> PathBuf;
  ```
- The manifest is written **before** each item moves and updated after it, so a failure mid-way leaves every moved item recorded. Registry export format and service/task export are the builder's choice, provided restore round-trips exactly (every value type: REG_SZ, EXPAND_SZ, MULTI_SZ, DWORD, QWORD, BINARY, NONE). Task export: the XML file from `System32\Tasks`; task delete/restore through the Task Scheduler COM API (`windows` crate), not `schtasks.exe` output. Service: export its registry key; delete/recreate through the Service Control Manager API.
- The machine root is created with an ACL granting only Administrators and SYSTEM (and the creator owner is Administrators).

- [ ] **Step 1:** Tests against a temp dir and an in-memory `Mutate`: `hold_then_restore_round_trips` (files, a folder tree, a key with every value type); `a_locked_file_is_named_and_everything_else_is_kept_restorable` (Review Focus 4: the fake fails one move; assert manifest lists the others and restore returns them); `restore_refuses_where_something_new_is_there`; `empty_older_than_30_days_leaves_newer_entries`; `list_marks_old_machine_entries_waiting`.
- [ ] **Step 2:** Run; FAIL. **Step 3:** Implement. **Step 4:** PASS.
- [ ] **Step 5:** Commit `Cleanup: the holding area, and putting things back`.

### Task 7: Internal steps, the closed list, the helper and the Runner

**Files:**
- Create: `crates/brokey-core/src/cleanup/internal.rs`
- Modify: `crates/brokey-core/src/transaction/allow.rs`, `crates/brokey-core/src/transaction/runner.rs`, `crates/brokey-helper/src/main.rs`

**Interfaces:**
- Produces, in `internal.rs`:
  ```rust
  pub const HOLD: &str = "brokey:hold";            // args[0] = JSON HoldPayload { holding: String, name: String, items: Vec<HoldItem> }
  pub const RESTORE: &str = "brokey:restore";      // args[0] = holding id
  pub const EMPTY: &str = "brokey:empty";          // args[0] = holding id or "all"
  pub const MSI_UNREGISTER: &str = "brokey:msi-unregister"; // args[0] = ProductCode GUID
  pub const AWAIT_GONE: &str = "brokey:await-gone"; // args[0] = JSON Hints; session only
  pub const SCAN: &str = "brokey:scan";            // args[0] = JSON { hints, force: bool }; session only
  pub const SHELL_OPEN: &str = "brokey:shell-open"; // args = [program, args...]; session only
  pub fn is_internal(program: &str) -> bool;
  pub fn run(step: &Step, ctx: &mut InternalContext) -> Result<Option<String>, String>; // Ok(note)
  ```
  `InternalContext` gives access to the sink (for `Event::Waiting` / `Event::Leftovers`), the cancel flag, a finished flag (set by the "It has finished" command), and `Scans` (`Arc<Mutex<HashMap<String, Findings>>>`, plus the hints per scan).
- `allow.rs`: `check_internal(step) -> Result<(), String>` called from `check_step` for internal programs. Root steps may only be `HOLD`, `RESTORE`, `EMPTY`, `MSI_UNREGISTER`. For `HOLD` every item: absolute, no `..`, not `guard::is_protected` (use the live view in the helper), **no name-surrogate reparse point at any component of the path** (checked by opening each component with `FILE_FLAG_OPEN_REPARSE_POINT`), registry items only under HKLM/HKLM32, holding id `[a-z0-9-]{1,64}`. `RESTORE` re-checks every destination in the manifest by the same rules. `MSI_UNREGISTER`: a GUID, and only keys named by its packed code. Session-only internals in a root step are refused.
- Runner `run_session`: if `is_internal(program)`, call `internal::run` instead of spawning. Helper `run_step`: same for root internals, after validation.

- [ ] **Step 1:** Tests in `allow.rs` next to the existing closed-list tests: refuse a HOLD of `C:\Windows\System32`, of `C:\Program Files`, of a relative path, of a path with `..`, of an HKCU key in a root step, of a malformed holding id, of `AWAIT_GONE` / `SCAN` / `SHELL_OPEN` in a root step, of an MSI_UNREGISTER with a non-GUID; **`a_hold_through_a_junction_is_refused`** (cfg(windows), create a temp dir with a junction inside it pointing at another temp dir; Review Focus 5); accept a well-formed HOLD of a temp-dir path.
- [ ] **Step 2:** Run `cargo test -p brokey-core allow`; FAIL. **Step 3:** Implement `internal.rs` (HOLD/RESTORE/EMPTY delegating to Task 6, MSI_UNREGISTER per spec §9, AWAIT_GONE/SCAN/SHELL_OPEN stubs returning Err until Task 8), `check_internal`, and the two dispatches. **Step 4:** PASS, plus the helper's own tests (`cargo test -p brokey-helper`).
- [ ] **Step 5:** Commit `Cleanup: internal steps on the closed list, run by the helper and the Runner`.

### Task 8: Planning: removal waits and scans, clearing, forcing, Steam, self-elevating uninstallers

**Files:**
- Modify: `crates/brokey-core/src/transaction/plan.rs`, `crates/brokey-core/src/cleanup/internal.rs`, `crates/brokey-core/src/sources/windows/arp.rs`, `crates/brokey-core/src/transaction/elevate/windows.rs`, `CLAUDE.md`

**Interfaces:**
- Consumes: `Store::leftover_hints` (Task 3), `scan`/`claims`/`findings` (Tasks 4–5), internals (Task 7), `allow::check_step`.
- Produces:
  - `plan.rs`: for every `Op::Remove` on Windows with `leftover_hints(...)` = Some, append session steps `AWAIT_GONE` then `SCAN` (titles: "Waiting for the uninstaller to finish", "Looking for what was left behind"). `Op::ForceRemove` → one `SCAN` step with `force: true` (no uninstaller). `Op::ClearLeftovers` → look the scan up in `Scans`, split ticked ids by `Scope` into one session `HOLD` and one root `HOLD` (same holding id), plus `MSI_UNREGISTER` when the findings came from a forced removal of an MSI entry. `Op::Restore` / `Op::EmptyHolding` → `RESTORE` / `EMPTY` steps, root only when that holding entry has machine items. The next plan that elevates for any other reason appends a root `EMPTY` for waiting machine entries.
  - `AWAIT_GONE`: poll `arp::read()` every 2 s for the uninstall key; when gone, Ok. While present after the previous step's process exited, emit `Event::Waiting { message: "Waiting for the uninstaller to finish. Close its window when it is done." }`; the finished flag triggers one last check: gone → Ok, present → Err("The uninstaller did not remove <name>. It may have been cancelled. Forced removal clears it without the uninstaller.").
  - `SCAN`: `LiveView`, `claims(view, store installed, excluding = force.then_some(&hints))`, `findings(...)`, store in `Scans`, emit `Event::Leftovers { count }`; zero items → note "Nothing was left behind."
  - `arp::removal`: an `UninstallString` of `"<…>\steam.exe" steam://uninstall/<digits>` → new `Removal::Steam(String)`; its step runs `steam.exe steam://uninstall/<id>` with `needs_root: false`.
  - Self-elevating uninstaller: in `plan.rs`, after a source returns a root removal step, run `allow::check_step`; if it errs with the `can_be_replaced` sentence and the step came from an Add/Remove entry, rewrite it as a session `SHELL_OPEN` step (title "Starting <name>'s uninstaller. Windows will ask you to allow it.") and add the PlanPreview sentence "Windows will ask you to allow <name>'s uninstaller, because Brokey does not start it as Administrator itself.".
  - `elevate/windows.rs`: `pub fn shell_open(program: &Path, args: &[String]) -> Result<std::process::ExitStatus-like, String>` using `ShellExecuteExW` verb `open`, `SEE_MASK_NOCLOSEPROCESS`, waiting on the handle. `SHELL_OPEN` calls it.
  - `CLAUDE.md` first invariant: add the second sanctioned exception, worded from spec §10's last bullet.

- [ ] **Step 1:** Tests: `removing_an_arp_entry_waits_then_scans` (plan steps' programs end with `AWAIT_GONE`, `SCAN`); `force_remove_runs_no_uninstaller`; `clear_splits_user_and_machine_items_into_two_holds`; `a_steam_game_is_removed_through_steam_as_the_user`; `an_uninstaller_in_a_writable_folder_is_started_through_shell_open` (inject the check result); `await_gone_with_the_entry_still_there_offers_forced_removal` (Review Focus 3; fake registry reader: finished flag set, entry present → Err sentence above, no `Event::Leftovers`).
- [ ] **Step 2:** Run; FAIL. **Step 3:** Implement. **Step 4:** PASS; full `cargo test --workspace`.
- [ ] **Step 5:** Commit `Cleanup: a removal waits for the uninstaller and scans; forced removal, Steam and self-elevating uninstallers`.

### Task 9: Commands and the text mode

**Files:**
- Modify: `crates/brokey/src/commands.rs`, `crates/brokey/src/state.rs`, `crates/brokey/src/cli.rs`, `frontend/src/api.ts`, `frontend/src/mock.ts`

**Interfaces:**
- Tauri commands: `leftovers_get(scan: String) -> Option<Findings>`, `await_finished(plan: String)`, `holding_list() -> Vec<HoldingEntry>`. Clearing, restoring, emptying and forcing go through the existing `plan` / `run_plan` with the new `Op`s.
- `AppState` holds the `Scans` map and hands it to the Runner.
- At start-up, on a background thread after the window opens, `holding::empty(&holding::user_root(), None, Some(30))`; machine entries are never touched at start-up (they become `waiting`).
- Text mode: `brokey leftovers <source>:<id>` prints a table (Kind, Location, Size, Confidence, Ticked) then the kept lines with who uses them, then the cloud/unreadable counts; it scans an application that is **still installed** (with `excluding` = itself, so its live files show as Certain), changes nothing, exits 0; unknown ref exits 1 with a sentence.
- Mock: `?leftovers=1` makes a removal in the mock end with a Findings sample (Discord, as in spec §6) and `holding_list` return two entries, one waiting.

- [ ] **Step 1:** Tests in `cli.rs`: `leftovers_needs_one_reference`, `leftovers_table_has_the_confidence_words`. Contract samples already exist (Task 1).
- [ ] **Step 2–4:** FAIL, implement, PASS. Then run `cargo run -p brokey -- leftovers winget:Discord.Discord` (or any installed app on the machine) and record the output in the commit message body.
- [ ] **Step 5:** Commit `Cleanup: commands for the page and brokey leftovers for a terminal`.

### Task 10: The page

**Files:**
- Create: `frontend/src/activity/Leftovers.tsx`, `frontend/src/pages/system/Holding.tsx` (or the nearest existing Settings sub-section pattern)
- Modify: `frontend/src/activity/store.ts`, `frontend/src/activity/ActivityPanel.tsx`, `frontend/src/activity/flow.ts`, `frontend/src/pages/AppDetail.tsx`, `frontend/src/pages/Settings.tsx`, `frontend/src/app.css`

**Interfaces:** Consumes the Task 1 types and Task 9 commands.

- The list, as spec §6's sketch: groups Files and folders / Registry / Shortcuts / Services and tasks; each row the painted checkbox, the location (middle-truncated, full on hover with the reason), size, confidence `Badge`; header "Leftovers from <name>" with count and total size; footer "N kept because other applications use them." (names on hover), cloud and unreadable counts as sentences; buttons **Keep everything** and **Move N to holding area** (runs `ClearLeftovers` with the ticked ids). Zero items: "Nothing was left behind."
- `Event::Waiting`: the step shows the empty rail and the message, with **It has finished** (`await_finished`) and **Stop waiting** (`cancel_plan`).
- When a removal plan fails with the forced-removal sentence, and on any Add/Remove edition's detail page behind the overflow/secondary action: **Forced removal…** → confirm dialog: "Forced removal does not run <name>'s uninstaller. Brokey removes its folder and its registry entry and shows what else it left, for you to choose." → `ForceRemove`.
- Settings: a "Holding area" section: each entry name, date, size, Restore, Delete now; "Empty" for all; the sentence "Brokey empties anything older than 30 days by itself. Items that need Administrator wait until the next change that asks for it, or until you press Empty." Waiting entries say "Waiting to be emptied".
- CSS: one block in `app.css` prefixed `lo-`, tokens only.

- [ ] **Step 1:** `npm run dev`, open `http://localhost:1420/?leftovers=1&platform=windows`, remove an app in the mock: the list renders; screenshot to the scratchpad.
- [ ] **Step 2:** `npm run lint` and `npm run check:design`. Expected: clean.
- [ ] **Step 3:** Commit `Cleanup: the leftovers list, the waiting buttons, forced removal and the holding area on the page`.

### Task 11: Docs, the real machine, and the release

**Files:** `README.md` (a "Leftovers" section; "What is not there yet": Linux cleanup comes next), `CHANGELOG.md` (`## 0.1.8` bullets), `docs/superpowers/specs/2026-09-19-windows-support-design.md` (Open: the "how the second spec hooks in" item closed, pointing at the new spec), `docs/superpowers/specs/2026-11-24-leftover-cleanup-design.md` ("As built" notes where the build departed).

- [ ] **Step 1:** On the reference machine, `cargo run -p brokey -- leftovers <ref>` for three installed applications (one MSI, one NSIS, one Steam game); check by hand that every offered item belongs to that application and that nothing protected or shared appears. Fix any false positive with a fixture before going on.
- [ ] **Step 2:** All gates from Global Constraints. Expected: all pass.
- [ ] **Step 3:** Commit the docs.
- [ ] **Step 4 (lead only, not a subagent):** `sh tools/release.sh 0.1.8 --dry-run`, then `sh tools/release.sh 0.1.8`, which pushes main, waits for CI and pushes the tag; the Release workflow publishes. Update the README download table to 0.1.8 in a follow-up commit as 0.1.7's was.
