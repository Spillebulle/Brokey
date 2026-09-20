# Windows metadata and the remaining sources: implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Windows applications get real icons and real descriptions, and
Chocolatey and Scoop join winget and Add/Remove Programs as sources.

**Architecture:** An icon is extracted from the bytes of the file
`DisplayIcon` names, reassembled as an `.ico`, and cached where the asset
protocol can already serve it. A description for a winget-known package comes
from that package's manifest in `winget-pkgs`, reached by a path the index
already tells us. Chocolatey and Scoop follow `arp.rs`: read the state the
tool keeps on disk, return `Step`s, run nothing.

**Tech Stack:** Rust 2024, `quick-xml`, `serde_json`, `reqwest` (blocking),
`rusqlite`, `zip`. No new image dependency: the extractor moves bytes and
decodes nothing.

**Spec:** `docs/superpowers/specs/2026-09-19-windows-support-design.md`, in
particular "Metadata and icons, measured", which supersedes "Metadata and
icons" where the two disagree.

**Not in this plan.** The Microsoft Store and MSIX. Enumerating Appx packages
needs the WinRT `PackageManager`: the registry repository over-reports by 5.6
times on the reference machine (964 entries against 171 real ones), and
`StateRepository-Machine.srd` is not readable by a user. That is a different
class of work from every task here, and nothing here depends on it. Windows
optional features stay out for the same reason: `DISM` is the read, and the
page slot it takes is a separate question from where software comes from.

**One file every task touches.** Tasks 1 to 5 each add a single `pub mod`
line to `crates/brokey-core/src/sources/windows/mod.rs`. Add your line; never
rewrite the file, and never remove a line a previous task added.

## Global Constraints

- Rust edition 2024, `rust-version = "1.88"`. No new workspace dependency
  without saying in the commit message why an existing one cannot do it.
- CI builds Linux with `RUSTFLAGS: -D warnings`. An item live on one platform
  and dead on the other fails that build. Everything pure goes outside a
  `#[cfg]`; only calls into the Windows API carry one.
- **A source never runs anything.** `plan()` returns `Step`s; the Runner
  executes them.
- **The window never elevates.** No elevation call site outside
  `crates/brokey-core/src/transaction/elevate/`, and the one documented
  exception in `CLAUDE.md`.
- **No test may elevate,** prompt for UAC, run `msiexec`, or write to HKLM.
  Reading HKLM and reading files under `%SystemRoot%` is fine.
- Copy rules, in code comments, doc comments, user-facing strings and commit
  messages alike: British spelling, sentence case, full stops, no em dashes,
  no emoji. Error strings are whole sentences saying what could not be done.
- Never fabricate a test transcript. A step that says "run it" means run it
  and paste what came back.
- Do not `git commit --amend`, `rebase` or `reset`, do not touch commits you
  did not create, and do not push.

---

### Task 1: The icon reader

An `.ico` file and a PE's icon resources are the same directory in two widths.
Turning one into the other copies image bytes untouched. Nothing is decoded,
so this is a pure function and CI runs it on Linux.

**Files:**
- Create: `crates/brokey-core/src/sources/windows/pe.rs`
- Modify: `crates/brokey-core/src/sources/windows/mod.rs` (add `pub mod pe;`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  /// Which icon group to take out of a PE.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  pub enum Wanted {
      /// `app.exe,2`: the third group in ascending id order.
      Nth(usize),
      /// `app.exe,-101`: the group whose resource id is 101.
      Id(u16),
  }

  /// The icon `wanted` names, as the bytes of an `.ico` file.
  pub fn icon(bytes: &[u8], wanted: Wanted) -> Option<Vec<u8>>;

  pub const RT_ICON: u32 = 3;
  pub const RT_GROUP_ICON: u32 = 14;
  ```

**Background the implementer needs.** A PE begins with `MZ`; the 4-byte
little-endian value at 0x3C is the offset of `PE\0\0`. The COFF header follows
it: number of sections at +2 (u16), size of the optional header at +16 (u16).
The optional header begins 20 bytes after the COFF header; its first u16 is
0x10b for PE32 and 0x20b for PE32+. The data directory begins 0x60 bytes into
the optional header for PE32 and 0x70 for PE32+, and its third entry (index 2,
so +16 bytes) is the resource directory's RVA and size. The section table
begins immediately after the optional header, 40 bytes per section, with
virtual size at +8, virtual address at +12, raw size at +16 and raw pointer at
+20. An RVA becomes a file offset by finding the section containing it and
adding the difference to that section's raw pointer.

A resource directory is 16 bytes, of which the last two u16 are the count of
named entries and then the count of id entries, followed by that many 8-byte
entries of `(name, offset)`. The high bit of `name` set means a string name,
which this reader skips. The high bit of `offset` set means another directory
at `resource_base + (offset & 0x7FFF_FFFF)`; clear means a data entry at
`resource_base + offset`, whose first two u32 are the data's RVA and its size.
Resources nest three deep: type, then id, then language.

A `RT_GROUP_ICON` is 6 header bytes then 14 bytes per image: width, height,
colour count and a reserved byte, then planes and bit count as u16, then the
size as u32 and the `RT_ICON` id as u16. An `.ico` file is the same 6 header
bytes then **16** bytes per image: the same first 12, then the size as u32 and
the image's offset within the file as u32. So the conversion appends each
named `RT_ICON`'s bytes and records where each one landed.

Every offset in this format comes from the file being parsed. Check each one
is in bounds before reading, and return `None` rather than panic.

- [ ] **Step 1: Write the failing tests**

Write `crates/brokey-core/src/sources/windows/pe.rs` with the constants, the
`Wanted` enum, a `todo!()` body for `icon`, and this test module. The tests
build their own PE, so no binary enters the repository:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A PE carrying exactly the resources asked for. Built rather than
    /// checked in: the parser is what is under test, and a synthesised file
    /// says precisely which bytes produced which answer.
    struct Builder {
        /// `(type_id, resource_id, bytes)`, in the order they are laid out.
        resources: Vec<(u32, u16, Vec<u8>)>,
        plus: bool,
    }

    impl Builder {
        fn new(plus: bool) -> Builder {
            Builder { resources: Vec::new(), plus }
        }

        fn add(mut self, type_id: u32, id: u16, bytes: Vec<u8>) -> Builder {
            self.resources.push((type_id, id, bytes));
            self
        }

        /// One `RT_GROUP_ICON` naming `entries`, each `(pixels, icon_id)`.
        fn group(self, id: u16, entries: &[(u8, u16)]) -> Builder {
            let mut g = Vec::new();
            g.extend_from_slice(&0u16.to_le_bytes());
            g.extend_from_slice(&1u16.to_le_bytes());
            g.extend_from_slice(&(entries.len() as u16).to_le_bytes());
            for (size, icon_id) in entries {
                g.push(*size);
                g.push(*size);
                g.push(0);
                g.push(0);
                g.extend_from_slice(&1u16.to_le_bytes());
                g.extend_from_slice(&32u16.to_le_bytes());
                g.extend_from_slice(&(*size as u32 * 4).to_le_bytes());
                g.extend_from_slice(&icon_id.to_le_bytes());
            }
            self.add(RT_GROUP_ICON, id, g)
        }

        /// Lay the whole thing out. The resource section's virtual address
        /// is deliberately not its file offset, so a reader that confuses
        /// the two fails every test below.
        fn build(self) -> Vec<u8> {
            todo!("written in step 3")
        }
    }

    /// The pixel width of each image an `.ico` carries, in order.
    fn sizes(ico: &[u8]) -> Vec<u8> {
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        (0..count).map(|i| ico[6 + i * 16]).collect()
    }

    fn offset_of(ico: &[u8], n: usize) -> usize {
        let at = 6 + n * 16 + 12;
        u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]) as usize
    }

    #[test]
    fn a_group_becomes_an_ico_carrying_every_image_it_names() {
        let pe = Builder::new(true)
            .add(RT_ICON, 1, vec![0xAA; 64])
            .add(RT_ICON, 2, vec![0xBB; 128])
            .group(7, &[(16, 1), (32, 2)])
            .build();
        let ico = icon(&pe, Wanted::Nth(0)).expect("the group is there");
        assert_eq!(&ico[0..6], &[0, 0, 1, 0, 2, 0], "an ICONDIR naming two images");
        assert_eq!(sizes(&ico), vec![16, 32]);
        let first = offset_of(&ico, 0);
        let second = offset_of(&ico, 1);
        assert_eq!(ico[first], 0xAA);
        assert_eq!(ico[second], 0xBB);
        assert_eq!(second, first + 64, "the second image follows the first");
        assert_eq!(first, 6 + 2 * 16, "the images follow the directory");
    }

    #[test]
    fn a_negative_index_names_a_resource_id_and_a_positive_one_counts() {
        let pe = Builder::new(true)
            .add(RT_ICON, 1, vec![0xAA; 64])
            .add(RT_ICON, 2, vec![0xBB; 64])
            .group(101, &[(16, 1)])
            .group(9, &[(32, 2)])
            .build();
        // Ascending id order, so group 9 is the first and group 101 the second.
        assert_eq!(sizes(&icon(&pe, Wanted::Nth(0)).unwrap()), vec![32]);
        assert_eq!(sizes(&icon(&pe, Wanted::Nth(1)).unwrap()), vec![16]);
        assert_eq!(sizes(&icon(&pe, Wanted::Id(101)).unwrap()), vec![16]);
        assert_eq!(icon(&pe, Wanted::Id(55)), None);
        assert_eq!(icon(&pe, Wanted::Nth(2)), None);
    }

    /// 32-bit binaries are still everywhere on Windows, and their data
    /// directory sits sixteen bytes earlier than a 64-bit one's.
    #[test]
    fn a_32_bit_binary_reads_the_same_as_a_64_bit_one() {
        let make = |plus| {
            Builder::new(plus)
                .add(RT_ICON, 1, vec![0xCC; 48])
                .group(1, &[(48, 1)])
                .build()
        };
        assert_eq!(
            icon(&make(false), Wanted::Nth(0)),
            icon(&make(true), Wanted::Nth(0))
        );
    }

    /// A group naming an icon the file does not carry is not a reason to
    /// lose the icons it does carry.
    #[test]
    fn an_image_the_group_names_but_the_file_lacks_is_dropped() {
        let pe = Builder::new(true)
            .add(RT_ICON, 1, vec![0xAA; 64])
            .group(1, &[(16, 1), (32, 404)])
            .build();
        assert_eq!(sizes(&icon(&pe, Wanted::Nth(0)).unwrap()), vec![16]);
    }

    #[test]
    fn a_group_naming_nothing_present_is_no_icon_at_all() {
        let pe = Builder::new(true).group(1, &[(16, 404)]).build();
        assert_eq!(icon(&pe, Wanted::Nth(0)), None);
    }

    #[test]
    fn a_file_with_no_resources_at_all_is_no_icon() {
        let pe = Builder::new(true).build();
        assert_eq!(icon(&pe, Wanted::Nth(0)), None);
    }

    #[test]
    fn rubbish_is_refused_rather_than_panicked_over() {
        assert_eq!(icon(b"", Wanted::Nth(0)), None);
        assert_eq!(icon(b"MZ", Wanted::Nth(0)), None);
        assert_eq!(icon(&vec![0x4D; 4096], Wanted::Nth(0)), None);
        // A PE cut in half, so its resource directory points past the end.
        let mut pe = Builder::new(true)
            .add(RT_ICON, 1, vec![0xAA; 64])
            .group(1, &[(16, 1)])
            .build();
        let half = pe.len() / 2;
        pe.truncate(half);
        assert_eq!(icon(&pe, Wanted::Nth(0)), None);
    }

    /// The machine's own binaries are the only test of the real format.
    /// Skipped where the file is absent rather than failed, so the suite
    /// stays green on Linux and inside a container.
    #[cfg(windows)]
    #[test]
    fn a_real_system_binary_yields_a_real_icon() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let path = std::path::Path::new(&root).join("explorer.exe");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipped: {} is not readable", path.display());
            return;
        };
        let ico = icon(&bytes, Wanted::Nth(0)).expect("explorer.exe has an icon");
        assert_eq!(&ico[0..4], &[0, 0, 1, 0], "an ICONDIR for an icon");
        assert!(ico.len() > 1024, "an icon of {} bytes is not one", ico.len());
        eprintln!("explorer.exe gave {} bytes, sizes {:?}", ico.len(), sizes(&ico));
    }
}
```

- [ ] **Step 2: Run the tests to watch them fail**

Run: `cargo test -p brokey-core --lib sources::windows::pe`
Expected: every test fails on `todo!()`.

- [ ] **Step 3: Write the reader, and `Builder::build`**

Implement `icon`, the private helpers it needs, and the builder.

Requirements, each of which a test above already asserts:
- `Wanted::Nth(n)` takes the `n`th group in **ascending resource id** order,
  which is the order Windows itself uses for a positive index.
- `Wanted::Id(n)` takes the group whose resource id is `n`.
- Language is not selected on: take the first language entry under an id.
- Skip named (string) resource entries; only integer ids are looked at.
- Drop a group entry whose `RT_ICON` is absent, and return `None` only when
  none of them is present.
- Never index a slice without a bounds check. Prefer `get(..)` and `?`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p brokey-core --lib sources::windows::pe`
Expected: PASS. Paste the output into the report, including the line the
real-binary test prints, because that line is the only evidence the reader
works on a file it did not build itself.

- [ ] **Step 5: Check the gates**

Run: `cargo fmt --all -- --check` then `cargo clippy --all-targets -- -D warnings`
Expected: both clean.

- [ ] **Step 6: Commit**

```bash
git add crates/brokey-core/src/sources/windows/pe.rs crates/brokey-core/src/sources/windows/mod.rs
git commit -m "Windows: an icon comes out of a binary as an .ico"
```

---

### Task 2: Cached icons, and the index that was being thrown away

`arp.rs` already returns a `Picture::File` pointing into Program Files. Two
things stop it drawing, and both had to be found before either mattered: the
asset protocol's scope does not admit that path, and the file it names is
usually a binary rather than a picture. Both are answered by writing an `.ico`
into the cache, which the scope already admits through `$CACHE/**`.

**Files:**
- Create: `crates/brokey-core/src/sources/windows/icon.rs`
- Modify: `crates/brokey-core/src/sources/windows/arp.rs` (the `icon` function
  at about line 232, and `to_package` at about line 288)
- Modify: `crates/brokey-core/src/sources/windows/mod.rs` (add `pub mod icon;`)

**The path from a cached file to a drawn picture, checked end to end.** Every
link below was read in the dependency sources rather than assumed, because
the assumption is what cost this branch a release:

1. `Picture::File(p)` reaches the page as `{kind: "file", value: p}`, and
   `frontend/src/api.ts` puts it through Tauri's `convertFileSrc`.
2. The asset protocol's scope in `crates/brokey/tauri.conf.json` lists
   `$CACHE/**`. Tauri resolves `$CACHE` with `dirs::cache_dir()`
   (`tauri-2.11.5/src/path/desktop.rs:53`), which on Windows is
   `{FOLDERID_LocalAppData}`, so `C:\Users\<user>\AppData\Local`.
3. Brokey's own cache is `directories::ProjectDirs`' cache directory,
   `%LOCALAPPDATA%\spillebulle\brokey\cache`, which is under that.
4. The protocol types the reply by sniffing and by extension, and `.ico` is
   `image/vnd.microsoft.icon` (`tauri-utils/src/mime_type.rs:34`), which
   WebView2 renders in an `<img>`.
5. The CSP already allows `img-src ... asset: http://asset.localhost ...`.

**None of that is evidence the picture draws.** It is five reasons to expect
it to, and step 7 is where you find out. If the window shows nothing, this
list is where to look, one link at a time.

**Interfaces:**
- Consumes: `pe::{icon, Wanted}` from Task 1.
- Produces:
  ```rust
  /// What a `DisplayIcon` value points at.
  #[derive(Clone, Debug, PartialEq, Eq)]
  pub struct Reference {
      pub path: PathBuf,
      pub wanted: pe::Wanted,
  }

  /// Split a `DisplayIcon` value into a file and which icon inside it.
  pub fn reference(display_icon: &str) -> Option<Reference>;

  /// The cached `.ico` for a reference, extracting it the first time.
  /// `None` when the file is gone, unreadable, or carries no icon.
  pub fn cached(cache: &Path, reference: &Reference) -> Option<PathBuf>;
  ```

- [ ] **Step 1: Write the failing tests for `reference`**

Every value below was taken from the reference machine's registry and is
exactly as it is stored there:

```rust
#[test]
fn a_display_icon_splits_into_a_file_and_which_icon_in_it() {
    let cases = [
        // A bare path, so the first icon.
        ("C:\\Program Files\\Cheat Engine\\Cheat Engine.exe",
         "C:\\Program Files\\Cheat Engine\\Cheat Engine.exe", pe::Wanted::Nth(0)),
        // The common form: a path and a zero.
        ("C:\\Program Files\\Vortex\\Vortex.exe,0",
         "C:\\Program Files\\Vortex\\Vortex.exe", pe::Wanted::Nth(0)),
        // Quoted, with the index outside the quotes.
        ("\"C:\\Program Files\\Microsoft OneDrive\\OneDrive.App.exe\",1",
         "C:\\Program Files\\Microsoft OneDrive\\OneDrive.App.exe", pe::Wanted::Nth(1)),
        // A negative index is a resource id, not a position.
        ("C:\\Program Files\\Microsoft OneDrive\\OneDriveSetup.exe,-101",
         "C:\\Program Files\\Microsoft OneDrive\\OneDriveSetup.exe", pe::Wanted::Id(101)),
        // An 8.3 path, which Windows opens as it is.
        ("C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE,0",
         "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE", pe::Wanted::Nth(0)),
        // An .ico file, which carries its own icons and needs no index.
        ("C:\\Program Files\\Git\\mingw64\\share\\git\\git-for-windows.ico",
         "C:\\Program Files\\Git\\mingw64\\share\\git\\git-for-windows.ico", pe::Wanted::Nth(0)),
        // A comma inside a directory name is not an index.
        ("C:\\Program Files\\Acme, Inc\\app.exe",
         "C:\\Program Files\\Acme, Inc\\app.exe", pe::Wanted::Nth(0)),
        // Surrounding space is trimmed.
        ("  C:\\a\\b.exe , 3 ", "C:\\a\\b.exe", pe::Wanted::Nth(3)),
    ];
    for (raw, path, wanted) in cases {
        let r = reference(raw).unwrap_or_else(|| panic!("{raw} gave nothing"));
        assert_eq!(r.path, PathBuf::from(path), "path of {raw}");
        assert_eq!(r.wanted, wanted, "index of {raw}");
    }
}

#[test]
fn nothing_to_point_at_is_no_reference() {
    assert_eq!(reference(""), None);
    assert_eq!(reference("   "), None);
    assert_eq!(reference(",0"), None);
    assert_eq!(reference("\"\""), None);
}
```

Then the cache tests, which call no Windows API and so run on Linux too.
Write all five out in full, in the same style:

- `an_ico_on_disk_is_copied_into_the_cache_rather_than_parsed` — a file whose
  first four bytes are `00 00 01 00` is copied through byte for byte, and the
  returned path starts with the cache directory.
- `a_binary_is_extracted_once_and_read_from_the_cache_after` — call `cached`
  twice, delete the source between the calls, and assert the second call still
  answers with the same path.
- `a_file_that_is_not_there_gives_no_picture`.
- `a_file_with_no_icon_in_it_gives_no_picture` — a file of random bytes.
- `the_cache_name_is_the_path_and_the_index_together` — the same path at two
  indices gives two files, and the same path and index twice gives one.

Use whatever temporary-directory helper the crate's tests already use; look at
`sources/linux/github.rs` or `http.rs` first, and add a small private one only
if there is none.

- [ ] **Step 2: Run them to watch them fail**

Run: `cargo test -p brokey-core --lib sources::windows::icon`
Expected: FAIL.

- [ ] **Step 3: Implement `reference` and `cached`**

`reference` takes the last comma **only** when everything after it trims to an
integer; `-n` becomes `Wanted::Id(n)` and `n` becomes `Wanted::Nth(n)`. Strip
one matching pair of double quotes from the path, then trim. An empty path is
`None`. A negative index too large for a `u16` is `Wanted::Nth(0)`, because a
resource id cannot be larger than that.

`cached` names its file from a hash of the lowercased path together with the
index, under `cache.join("icons")`, and returns that path without touching the
source when the file is already there. Otherwise it reads the source: four
leading bytes of `00 00 01 00` mean it is already an `.ico` and it is copied
through; anything else goes to `pe::icon`. Write to a temporary name in the
same directory and rename into place, so a half-written file is never served.

Use the hash the crate already uses elsewhere if there is one; otherwise
`std::collections::hash_map::DefaultHasher` is enough, since this names a
cache entry and guards nothing.

- [ ] **Step 4: Run them**

Run: `cargo test -p brokey-core --lib sources::windows::icon`
Expected: PASS.

- [ ] **Step 5: Wire it into `arp.rs`**

`RawEntry` already carries `display_icon`. Change `arp::icon` to return the
cached path, and rewrite its doc comment: the present one says the page asks
the shell for the picture, which nothing does, and that belief is why the
index was being dropped. 52 of the reference machine's 112 `DisplayIcon`
values carry one.

`to_package` has no cache directory to hand. Give `Arp` a `cache: PathBuf`
field set from `crate::system::Dirs::new().cache` in `Arp::new()`, add an
`Arp::with_cache` for tests, and thread it into `to_package`. Keep the
existing `to_package` tests compiling by giving them a temporary directory.

Add one test that goes the whole way: a `RawEntry` whose `display_icon` names
a file the test wrote produces a `Package` whose `icon` is a `Picture::File`
under that cache directory.

- [ ] **Step 6: Run the crate's tests**

Run: `cargo test -p brokey-core`
Expected: PASS. Paste the summary line.

- [ ] **Step 7: Prove it in the window, because no test can**

Nothing in this repository opens the application window, so a green suite says
nothing about whether a picture draws. Build it and look:

```bash
npm run build
cargo build --release -p brokey
./target/release/brokey.exe
```

Open the Installed page. Report, in the report file: roughly how many rows
drew an icon, how many drew the initial block, and whether any drew a broken
image. If none drew, check in this order, and say which one it was: whether
files appeared under `%LOCALAPPDATA%\spillebulle\brokey\cache\icons`; whether
Tauri's `$CACHE` resolves to `%LOCALAPPDATA%` and so admits that directory;
and whether the asset protocol served the request at all. Report this step
from what the window showed, never from reading the configuration.

- [ ] **Step 8: Commit**

```bash
git add crates/brokey-core/src/sources/windows/
git commit -m "Windows: applications get their own icons"
```

---

### Task 3: Chocolatey

**Files:**
- Create: `crates/brokey-core/src/sources/windows/choco.rs`
- Create: `crates/brokey-core/tests/fixtures/choco/` (three files, below)
- Modify: `crates/brokey-core/src/sources/windows/mod.rs` (add `pub mod choco;`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pub struct Choco` implementing `crate::Source`, with
  `kind() == SourceKind::Choco`. `SourceKind::Choco` already exists and
  already has a label; nothing in `model.rs` needs changing.

Follow `crates/brokey-core/src/sources/windows/arp.rs` for the shape of a
Windows source, and `crates/brokey-core/src/sources/linux/snap.rs` for a
source that reads local state and searches a remote catalogue.

**What it reads.** Installed packages are directories under
`%ChocolateyInstall%\lib`, defaulting to `C:\ProgramData\chocolatey` when that
variable is unset, each holding `<id>.nuspec`. That file is XML whose
`<metadata>` element carries `id`, `version`, `title`, `authors`,
`description`, `summary`, `projectUrl`, `licenseUrl`, `iconUrl` and `tags`.
Parse it with `quick-xml`, which the workspace already has.

**What it searches.** The community feed, OData v2. The query was run before
this plan was written, and it is fussier than the obvious form: leaving off
`targetFramework` or `includePrerelease` answers 400 with "Error in query
syntax", not a useful message. Use exactly this, percent-encoding the query:

```
https://community.chocolatey.org/api/v2/Search()?$filter=IsLatestVersion&$top=<limit>&searchTerm='<query>'&targetFramework=''&includePrerelease=false
```

The reply is Atom XML, and **the package id is not where you would expect it.
There is no `d:Id` property at all.** In each `<entry>`:

- `<title type="text">` is the **id**, `7zip`.
- `<d:Title>` is the **display name**, `7-Zip`. These are different values and
  the page wants both.
- `<summary type="text">` is a one-line summary; `<d:Description>` is the long
  form and carries Markdown, including `##` headings and `-` lists.
- `<m:properties>` also holds `Version`, `IconUrl`, `ProjectUrl`,
  `LicenseUrl`, `PackageSize`, `DownloadCount`, `Tags` (space separated, not
  comma separated) and `Published`.
- `<id>` is an OData URL, not an id. Do not parse it for one; the title has it.

**Steps.** `choco install <id> -y`, `choco upgrade <id> -y` and
`choco uninstall <id> -y --remove-dependencies`. Every one needs
Administrator, because the default install root is under `C:\ProgramData`.

**Status.** Available when `choco.exe` is on the `PATH` or
`%ChocolateyInstall%\bin\choco.exe` exists. When it is not, the source stays
searchable the way winget does, because the feed is HTTP and needs no tool.

**`setup()` returns `None` in this plan, and that is deliberate.** The spec's
table says Chocolatey's bootstrap extracts `chocolatey.nupkg` into
`C:\ProgramData\chocolatey` and runs its bundled install script, elevated, and
the spec then adds an invariant that binds it: **nothing downloaded is run
before it is verified**, which for Chocolatey means checking that the
extracted `choco.exe` is Authenticode-signed by Chocolatey Software, Inc.
That is a `WinVerifyTrust` call and a new elevated path through the helper's
closed list, which is a larger and more security-sensitive piece of work than
every other task in this plan put together. It gets its own plan and its own
review.

So: `status()` says Chocolatey is not installed and `setup()` answers `None`,
which the page already knows how to draw, and the source is searchable
meanwhile. Write that reason into `setup()`'s doc comment so the next reader
finds it there rather than assuming it was forgotten. A test asserts
`setup().is_none()`, so restoring it is a deliberate act rather than an
accident.

- [ ] **Step 1: Put the fixtures in place**

The search fixture is real, was fetched from the feed above before this plan
ran, and waits in this plan's workspace. Copy it rather than fetching again:

```bash
W=.superpowers/sdd/2026-09-20-windows-metadata-and-sources
mkdir -p crates/brokey-core/tests/fixtures/choco
cp "$W/choco-search.xml" crates/brokey-core/tests/fixtures/choco/search.xml
```

It holds two entries, `7zip` and `GoogleChrome`, trimmed to the properties
this source reads. `7zip`'s `d:Description` carries Markdown headings, and its
`<title>` and `<d:Title>` differ, which is the case the reader gets wrong if
it takes the id from the wrong element.

Then write `tests/fixtures/choco/googlechrome.nuspec` and
`tests/fixtures/choco/7zip.nuspec` by hand, each a short but real-shaped
`.nuspec`. Give one of them a `<title>` and an `<iconUrl>` and the other
neither, so the fallbacks below have something to fall back from.

- [ ] **Step 2: Write the failing tests**

All pure, so they run on Linux:
- `a_nuspec_becomes_a_package` — id, version, title, description, developer,
  homepage, and `installed == true`.
- `a_nuspec_without_a_title_falls_back_to_its_id`.
- `an_icon_url_becomes_a_url_picture` — `Picture::Url`, never `Picture::File`.
- `a_search_reply_becomes_packages` — against `search.xml`, asserting both
  entries.
- `the_id_comes_from_the_atom_title_and_the_name_from_d_title` — `7zip`
  against `7-Zip`, by name, because taking either from the other element is
  the mistake this source is most likely to make.
- `a_reply_that_is_not_xml_is_an_error_not_a_panic`.
- `every_operation_needs_administrator` — the exact argument vector of each
  step, and `needs_root == true` on all of them.

- [ ] **Step 3: Run them to watch them fail**

Run: `cargo test -p brokey-core --lib sources::windows::choco`

- [ ] **Step 4: Implement**

- [ ] **Step 5: Run them**

Run: `cargo test -p brokey-core`
Expected: PASS. Paste the summary line.

- [ ] **Step 6: Gates**

`cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 7: Commit**

```bash
git commit -m "Windows: Chocolatey is a source"
```

---

### Task 4: Scoop

**Files:**
- Create: `crates/brokey-core/src/sources/windows/scoop.rs`
- Create: `crates/brokey-core/tests/fixtures/scoop/` (two manifests)
- Modify: `crates/brokey-core/src/sources/windows/mod.rs` (add `pub mod scoop;`)

**Interfaces:**
- Produces: `pub struct Scoop` implementing `crate::Source`, with
  `kind() == SourceKind::Scoop`.

**What it reads.** `%SCOOP%`, or `~\scoop` when that is unset. Installed
applications are directories under `apps\<name>`, each with a `current`
junction holding `manifest.json`. Buckets are under
`buckets\<bucket>\bucket\*.json`. Parse with `serde_json`, which the
workspace already has.

**A manifest's fields do not have one shape each,** which the two real
manifests in the fixtures show. `license` is a plain string in `nodejs.json`
(`"MIT"`) and an object in `7zip.json` (`{"identifier": ..., "url": ...}`);
read both, taking the identifier out of the object. `bin` is a list in
`7zip.json`, absent altogether in `nodejs.json`, and documented as also being
a plain string or a list of lists (`[["path", "alias"]]`). A reader that
assumes one shape works until it meets the other, so handle every shape or
ignore the field, and never `unwrap` on its type.

**What it searches.** The buckets on disk when scoop is installed. When it is
not, the main bucket from GitHub. Two requests, both run before this plan was
written:

- The list of manifest names, once, from the bucket's git tree:
  `https://api.github.com/repos/ScoopInstaller/Main/git/trees/<sha of bucket>`,
  where that sha comes from
  `https://api.github.com/repos/ScoopInstaller/Main/git/trees/master`. It
  answers 1,654 names in one 465 KB reply, untruncated, and matching then
  happens locally against it.
- Then one manifest per match, from
  `https://raw.githubusercontent.com/ScoopInstaller/Main/master/bucket/<name>.json`.

Cache both through `crate::http::Client`, the way the winget catalogue is
cached. A name that is not in Main answers 404 with an HTML body, so a failed
parse there means "not in this bucket", not "the source is broken".

**Steps.** `scoop install <name>`, `scoop update <name>` and
`scoop uninstall <name>`. **`needs_root` is false on every one of them, and a
test says so by name.** The spec's line is that Scoop never elevates, ever;
that is the whole point of Scoop, and a step of this source asking for
Administrator would be a defect rather than a setting.

**`setup()` returns `None` in this plan,** for the same reason Task 3's does.
The spec pins Scoop's installer to a named commit of `ScoopInstaller/Install`
rather than the redirecting `get.scoop.sh`, under the invariant that nothing
downloaded is run before it is verified. Scoop's bootstrap does not elevate,
so it is the smaller half of that work, but it is still fetching a script and
running it, and it belongs with Chocolatey's in the plan that builds the
verification rather than ahead of it. Say so in `setup()`'s doc comment and
assert `setup().is_none()` in a test.

- [ ] **Step 1: Put the fixtures in place, then write the failing tests**

Both manifests are real, were fetched before this plan ran, and wait in this
plan's workspace. Copy them rather than fetching again:

```bash
W=.superpowers/sdd/2026-09-20-windows-metadata-and-sources
mkdir -p crates/brokey-core/tests/fixtures/scoop
cp "$W/scoop-7zip.json" crates/brokey-core/tests/fixtures/scoop/7zip.json
cp "$W/scoop-nodejs.json" crates/brokey-core/tests/fixtures/scoop/nodejs.json
```

Tests, including by name:
- `a_manifest_becomes_a_package` — against `7zip.json`.
- `a_licence_is_read_whether_it_is_a_string_or_an_object` — `nodejs.json`
  gives `MIT` and `7zip.json` gives its identifier, down the same code path.
- `a_manifest_without_a_bin_is_not_an_error` — `nodejs.json` has none.
- `an_installed_app_reads_its_version_from_the_current_manifest`.
- `nothing_scoop_does_ever_needs_administrator` — every step of every
  operation, with the reason in the test's doc comment.
- `a_manifest_that_is_not_json_is_an_error_not_a_panic`.
- `a_bucket_with_no_matches_is_an_empty_list_not_an_error`.

- [ ] **Step 2: Run them to watch them fail**
- [ ] **Step 3: Implement**
- [ ] **Step 4: Run them** — `cargo test -p brokey-core`, PASS, paste it.
- [ ] **Step 5: Gates** — fmt and clippy.
- [ ] **Step 6: Commit** — `git commit -m "Windows: Scoop is a source"`

---

### Task 5: A winget package describes itself

The catalogue carries no description: its eleven tables were counted and there
is no description column and no icon column in any of them. The manifest does
carry one, and its path is derivable from what the index already holds.

**Files:**
- Create: `crates/brokey-core/src/sources/windows/winget/manifest.rs`
- Create: `crates/brokey-core/tests/fixtures/winget/gimp.locale.yaml`
- Modify: `crates/brokey-core/src/sources/windows/winget/mod.rs` (`details`)

**Interfaces:**
- Produces:
  ```rust
  /// What a locale manifest says about a package.
  #[derive(Clone, Debug, Default, PartialEq, Eq)]
  pub struct Described {
      pub short_description: Option<String>,
      pub description: Option<String>,
      pub publisher_url: Option<String>,
      pub package_url: Option<String>,
      pub license: Option<String>,
      pub license_url: Option<String>,
      pub release_notes_url: Option<String>,
      pub tags: Vec<String>,
  }

  /// Where `winget-pkgs` keeps the English manifest for one version.
  pub fn manifest_url(id: &str, version: &str) -> Option<String>;

  /// Read one. Not a general YAML reader: see the module's doc comment.
  pub fn parse(yaml: &str) -> Described;
  ```

**The path.** `GIMP.GIMP` at `3.2.4` is at
`https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests/g/GIMP/GIMP/3.2.4/GIMP.GIMP.locale.en-US.yaml`,
which was fetched and read before this plan was written. The first segment is
the lowercased first character of the id; then the id split on `.`, each part
its own directory; then the version; then the id followed by
`.locale.en-US.yaml`. An id of more than two parts,
`Microsoft.VisualStudio.2022.Community`, becomes four directories. An id whose
first character is neither an ASCII letter nor a digit has no derivable path,
so `manifest_url` answers `None` rather than guessing.

**The reader.** These manifests are a narrow, machine-generated subset of
YAML, and taking a YAML dependency to read eight scalars is not worth it.
Handle exactly what the schema produces: `Key: value` at the left margin, a
block scalar introduced by `Key: |` or `Key: >-` whose indented lines follow,
a sequence of `- item` lines under a bare `Key:`, and `#` comment lines.
Anything else is ignored rather than guessed at. Say all of that in the
module's doc comment, so the next person meets its limits before they hand it
a file it cannot read.

**Where it is used.** `Winget::details` only, never `search`: it is one HTTP
fetch per package, which a search page of twenty results cannot afford. Cache
it through `crate::http::Client` the way the catalogue is cached. A fetch that
fails leaves the package exactly as the index described it, because a missing
description is not an error the page should show.

- [ ] **Step 1: Put the fixtures in place**

Both were fetched and checked before this plan ran, and are waiting in this
plan's workspace. Copy them, do not fetch them again:

```bash
W=.superpowers/sdd/2026-09-20-windows-metadata-and-sources
mkdir -p crates/brokey-core/tests/fixtures/winget
cp "$W/gimp.locale.yaml" crates/brokey-core/tests/fixtures/winget/
cp "$W/blender.locale.yaml" crates/brokey-core/tests/fixtures/winget/
```

`gimp.locale.yaml` is `GIMP.GIMP` 3.2.4: a comment line, eleven tags, and a
`ShortDescription` on one long line with no `Description` at all.
`blender.locale.yaml` is `BlenderFoundation.Blender` 5.0.1, and it is the
harder of the two: two comment lines, a `Description: |-` block scalar of
three indented lines, a `Tags:` sequence, and a `Documentations:` key whose
value is a sequence of mappings. That last one is the shape this reader does
not handle, and the test below says what it does with it instead.

- [ ] **Step 2: Write the failing tests**

- `the_path_comes_from_the_id_and_the_version` — `GIMP.GIMP` at `3.2.4`,
  `Microsoft.VisualStudio.2022.Community` at `17.0`, and `7zip.7zip` at
  `24.09`, which begins with a digit.
- `an_id_that_cannot_be_a_path_gives_nothing` — `""`, `".x"`, an id with no
  dot at all, and one starting with a character that is not alphanumeric.
- `a_real_manifest_reads` — every field of `Described` against
  `gimp.locale.yaml`, `description` included, which is `None` there.
- `a_block_scalar_keeps_its_line_breaks_and_loses_its_indentation` — against
  `blender.locale.yaml`, whose `Description` is three lines. Assert the line
  count and that no line begins with a space.
- `a_nested_sequence_this_reader_does_not_handle_is_skipped_whole` — the
  `Documentations:` block of `blender.locale.yaml` must not leak into `tags`
  or into any scalar. Assert `tags` is exactly the six Blender tags.
- `a_manifest_missing_everything_optional_is_not_an_error` — a three-line
  input gives `Described::default()` but for what it does carry.
- `a_key_this_reader_does_not_know_is_ignored`.

- [ ] **Step 3: Run them to watch them fail**
- [ ] **Step 4: Implement**
- [ ] **Step 5: Run them** — PASS.

- [ ] **Step 6: Fill `details` from it**

In `winget/mod.rs`, `details` fetches the manifest for the id and the version
the index holds, and fills `description`, `homepage`, `license` and the facts
column from what comes back. Leave `search` untouched. Add a test that
`details` still answers when the fetch fails.

- [ ] **Step 7: Run the crate's tests, then fmt and clippy**

- [ ] **Step 8: Commit**

```bash
git commit -m "Windows: a winget package describes itself"
```

---

### Task 6: The sources join the application

Nothing above is reachable from the window until this task runs.

**Files:**
- Modify: `crates/brokey-core/src/sources/mod.rs` (`all`, and its Windows test)
- Modify: `CHANGELOG.md`
- Modify: `README.md` (the Windows bullet under "What is not there yet")

- [ ] **Step 1: Add them to `all`**

In interface order: `Arp`, `Winget`, `Choco`, `Scoop`. Update the existing
test `windows_has_add_remove_programs_and_winget`, its name included, to
assert all four in that order.

- [ ] **Step 2: Run the whole suite**

Run: `cargo test --workspace`
Expected: PASS. Paste the summary.

- [ ] **Step 3: Check the text mode sees them**

Run: `cargo run -p brokey -- search 7zip`
Expected: results naming more than one source. Paste what came back. Read
`--help` for the name of the subcommand that lists sources rather than
guessing at one, then run it and paste that too.

- [ ] **Step 4: Update the README's Windows bullet**

It currently says "Applications have no icons or descriptions there" and
"Chocolatey, Scoop and the Microsoft Store come next". Both are now partly
untrue. Write what is true after this plan: installed applications have their
own icons, a detail page shows the description from the winget manifest,
Chocolatey and Scoop are sources, and **the Microsoft Store is still to
come.** Do not write that search results have icons, because they do not.

- [ ] **Step 5: CHANGELOG**

Add an `## Unreleased` section. `crates/brokey/tests/release.rs` enforces its
shape, so read that test before writing, then run
`cargo test -p brokey --test release`.

- [ ] **Step 6: A source without a label fails a test**

`check.sh` is a shell script and cannot ask Rust anything, so this belongs in
`sources/mod.rs` beside the test above it: every kind that Windows `all`
returns has a non-empty `label()` and a `parse()` that round-trips it. A
source added without a label then fails the suite rather than drawing a blank
column in the page's source filter.

- [ ] **Step 7: Run every gate**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
npm run lint
npm run check:design
bash packaging/check.sh
```

Paste each result. A failure here is a finding to report, not something to
work around.

- [ ] **Step 8: Build and look at the window again**

The same as Task 2 step 7, and for the same reason. Search for something both
Chocolatey and Scoop carry. Report what the window showed: which sources
appeared in the filter, whether their rows drew, and whether a winget detail
page showed a description. Report it from the window.

- [ ] **Step 9: Commit**

```bash
git commit -m "Windows: four sources, and a page that says so"
```
