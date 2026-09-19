# The winget source Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Brokey on Windows searches winget's catalogue of 14,896 packages, says
which of them this machine already has, which have a newer version, and plans
the `winget.exe` steps that would install, update or remove one.

**Architecture:** The source reads Microsoft's pre-indexed catalogue itself
rather than parsing `winget search` output, exactly as `sources/linux/pacman.rs`
reads the sync tarball rather than parsing `pacman -Ss`. The catalogue is
`source2.msix` from the winget CDN: a plain zip wrapping a SQLite `index.db`.
Brokey downloads it, caches it and queries it with SQL. Because the index is a
file rather than a tool, winget is `searchable` on a machine that has never had
winget installed, which is the rule Flatpak and Snap already follow. Nothing in
this plan runs anything: `plan()` returns steps and a later plan's runner
executes them.

**Tech Stack:** Rust 2024, `rusqlite` with `bundled`, `zip`, the workspace's
`reqwest` client and its disk cache.

**Spec:** `docs/superpowers/specs/2026-09-19-windows-support-design.md`

**Scope.** This plan is the winget **source** only. The Windows privilege path
(the named pipe, `ShellExecuteEx`, the helper's Windows body, `allow.rs`'s
second closed list) is a separate plan, because it is a separate subsystem with
its own risk and its own reviewer. Until it lands, a Windows plan is built and
previewed but not run, which is the state Add/Remove Programs is already in.
Chocolatey, Scoop, MSIX and the Store, Windows optional features, the styled
installer and the 0.1.5 release are all out of scope.

---

## Global Constraints

Copied from the spec and from `CLAUDE.md`. Every task's requirements include
these.

- **A source never runs anything.** `plan()` returns steps; the Runner executes
  them. This is what makes every source testable with fixtures.
- **No shelling out to winget to parse its output.** The index is read
  directly. `winget.exe` appears only inside a `Command` in a `Step`.
- **Every source reports availability with a reason.** A source is never
  silently skipped. `status()` carries `available`, `reason`, `detail`,
  `searchable` and `setup`.
- **Pure halves stay ungated.** A module compiles on both platforms; only the
  functions that touch the operating system carry `#[cfg(windows)]`, so their
  tests run on Linux, which is where most of this project's tests actually run.
  This is the pattern `system/windows.rs` and `sources/windows/arp.rs` follow.
- **`std::path::Path` cannot parse Windows paths off Windows.** A backslash is
  an ordinary character there. Use text helpers, never `Path::parent`, in code
  that compiles on Linux.
- **Copy rules:** British spelling, sentence case, full stops in sentences, no
  em dashes, no emoji. Errors name what went wrong and what to do.
- **A heuristic arrives with a fixture where it fires and one where it must
  not.** This is the `group.rs` rule and it binds every ranking rule here.
- **Rust 2024, `rust-version = "1.88"`.** Let-chains are house style.
- **Never fabricate a test transcript.** An admitted gap is fine; an invented
  passing run is not.

## Evidence this plan rests on

Measured on 2026-09-19 against the live CDN and this machine's registry. A task
that contradicts one of these numbers is wrong, not the number.

| | |
|---|---|
| `https://cdn.winget.microsoft.com/cache/source2.msix` | 3,624,397 bytes |
| `Public/index.db` inside it | 8,409,088 bytes |
| Its `metadata` `majorVersion` / `minorVersion` | `2` / `0` |
| `source.msix`, the older index | 20,564,029 bytes, schema 1.7, 42 MB database |
| Packages in either | 14,896 |
| Uninstall keys on this machine | 423, of which 317 named, 157 applications |
| Applications joined to a winget package by product code | 68 |
| Joined by normalised name after that | 17 |
| Left unmatched | 72 |

Three properties of the data are not guessable, and the tasks below depend on
them.

1. **`source2.msix` is not a delta.** It is a complete index in a newer, flatter
   schema, holding the same 14,896 packages in a fifth of the bytes. Read it,
   and read `metadata` to confirm `majorVersion` is `2` before trusting any
   table name.
2. **Product codes are stored lower-case, and not all of them are GUIDs.**
   `7-zip` and `notepad++` sit in `productcodes2` beside
   `{23170f69-40c1-2701-1604-000001000000}`, because the column holds whatever
   the uninstall key is named. Fold case on both sides of the join and never
   assume a GUID.
3. **The index deliberately folds a version family to one name.** Every
   `Python.Python.3.0` through `Python.Python.3.14` carries the `norm_names2`
   row `python` and the `norm_publishers2` row `pythonsoftwarefoundation`. They
   are meant to be one application with fifteen editions, and `group.rs` is what
   makes them one, which is why Task 5 must populate `Package.developer`. A
   winget source that leaves `developer` empty ships fifteen separate rows for
   Python, and there is no later place to fix it.

## The real schema, for reference

Read off the live catalogue. Every SQL statement in this plan is written
against exactly this.

```sql
CREATE TABLE [packages](rowid INTEGER PRIMARY KEY, [id] TEXT NOT NULL,
  [name] TEXT NOT NULL, [moniker] TEXT, [latest_version] TEXT NOT NULL,
  [arp_min_version] TEXT, [arp_max_version] TEXT, [hash] BLOB);
CREATE TABLE [productcodes2]([productcode] TEXT NOT NULL, [package] INT64 NOT NULL, ...);
CREATE TABLE [upgradecodes2]([upgradecode] TEXT NOT NULL, [package] INT64 NOT NULL, ...);
CREATE TABLE [norm_names2]([norm_name] TEXT NOT NULL, [package] INT64 NOT NULL, ...);
CREATE TABLE [norm_publishers2]([norm_publisher] TEXT NOT NULL, [package] INT64 NOT NULL, ...);
CREATE TABLE [tags2](rowid INTEGER PRIMARY KEY, [tag] TEXT NOT NULL);
CREATE TABLE [tags2_map]([tag] INT64 NOT NULL, [package] INT64 NOT NULL, ...);
CREATE TABLE [metadata]([name] TEXT PRIMARY KEY NOT NULL, [value] TEXT NOT NULL);
```

There is no description, homepage, licence or size column. The index is a
search index; everything else comes from the manifest, and the metadata ladder
that fetches manifests is plan 3. Until then those fields are `None`, which is
what `Package` already expects of a source that does not know them.

## File Structure

```
crates/brokey-core/src/sources/windows/winget/
  mod.rs      the Source impl: status, search, installed, updates, details, plan, setup
  index.rs    the catalogue file: where it is cached, fetching it, unwrapping the
              zip, checking the schema version, opening the connection
  query.rs    every SQL statement and the row to Package mapping
  version.rs  comparing two Windows version strings
crates/brokey-core/tests/fixtures/windows/winget/
  source2.msix   a small zip of the same shape as the real one, built by the
                 script in Task 1 and committed
tools/make-winget-fixture.py   builds that fixture, so it is rebuilt rather
                               than hand-patched
```

`mod.rs` holds the trait impl and nothing else; the three modules beside it are
pure and are where the tests live. `arp.rs` stays one file because it is one
pure function over a fixture. winget is four responsibilities and is split.

---

### Task 1: The catalogue file: fetch, unwrap, open

**Files:**
- Create: `crates/brokey-core/src/sources/windows/winget/index.rs`
- Create: `crates/brokey-core/src/sources/windows/winget/mod.rs`
- Create: `tools/make-winget-fixture.py`
- Create: `crates/brokey-core/tests/fixtures/windows/winget/source2.msix`
- Modify: `crates/brokey-core/src/sources/windows/mod.rs`
- Modify: `Cargo.toml`, `crates/brokey-core/Cargo.toml`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub const CATALOGUE_URL: &str`
  - `pub const DATABASE_MEMBER: &str`
  - `pub const SCHEMA_MAJOR: i64`
  - `pub const MAX_AGE: std::time::Duration`
  - `pub fn database_from_msix(bytes: &[u8]) -> crate::Result<Vec<u8>>`
  - `pub fn schema_major(db: &rusqlite::Connection) -> crate::Result<i64>`
  - `pub fn open(path: &std::path::Path) -> crate::Result<rusqlite::Connection>`
  - `pub fn cached_path(cache_dir: &std::path::Path) -> std::path::PathBuf`

**Dependency note.** `rusqlite`'s `bundled` feature compiles SQLite from C. The
spec allows this explicitly: the objection in `docs/architecture.md` was to
libalpm and libapt, which tie one binary to one distribution's library version,
and a statically bundled SQLite is the opposite, adding no runtime dependency
and being identical everywhere. It does mean the workspace needs a C compiler
on every build host. All three CI runners have one. Do not try to cross-compile
locally; it does not work on the development machine for unrelated reasons.

- [ ] **Step 1: Add the dependencies**

In `Cargo.toml` under `[workspace.dependencies]`, after `regex`:

```toml
# The winget catalogue is a SQLite database inside a zip. `bundled` compiles
# SQLite from source rather than linking the host's: the objection in
# docs/architecture.md was to libalpm and libapt, which tie one binary to one
# distribution's library version. A statically bundled SQLite is the opposite.
rusqlite = { version = "0.37", features = ["bundled"] }
# Only to unwrap that one member. No encryption, no zip64 writing.
zip = { version = "6", default-features = false, features = ["deflate"] }
```

In `crates/brokey-core/Cargo.toml` under `[dependencies]`:

```toml
rusqlite = { workspace = true }
zip = { workspace = true }
```

- [ ] **Step 2: Write the fixture builder**

Create `tools/make-winget-fixture.py`:

```python
#!/usr/bin/env python3
"""Build the small winget catalogue the tests read.

    python3 tools/make-winget-fixture.py

Writes crates/brokey-core/tests/fixtures/windows/winget/source2.msix: a zip of
the same shape as Microsoft's, holding a SQLite database with the real schema
version 2 tables and eight packages chosen to exercise every ranking rule.

The real catalogue is 3.6 MB and is rebuilt daily, so it is not committed.
This is, so the tests are deterministic and run on Linux.
"""
from __future__ import annotations

import sqlite3
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "crates/brokey-core/tests/fixtures/windows/winget/source2.msix"

SCHEMA = """
CREATE TABLE [metadata]([name] TEXT PRIMARY KEY NOT NULL, [value] TEXT NOT NULL);
CREATE TABLE [packages](rowid INTEGER PRIMARY KEY, [id] TEXT NOT NULL,
  [name] TEXT NOT NULL, [moniker] TEXT, [latest_version] TEXT NOT NULL,
  [arp_min_version] TEXT, [arp_max_version] TEXT, [hash] BLOB);
CREATE TABLE [productcodes2]([productcode] TEXT NOT NULL, [package] INT64 NOT NULL,
  PRIMARY KEY([productcode], [package])) WITHOUT ROWID;
CREATE TABLE [upgradecodes2]([upgradecode] TEXT NOT NULL, [package] INT64 NOT NULL,
  PRIMARY KEY([upgradecode], [package])) WITHOUT ROWID;
CREATE TABLE [norm_names2]([norm_name] TEXT NOT NULL, [package] INT64 NOT NULL,
  PRIMARY KEY([norm_name], [package])) WITHOUT ROWID;
CREATE TABLE [norm_publishers2]([norm_publisher] TEXT NOT NULL, [package] INT64 NOT NULL,
  PRIMARY KEY([norm_publisher], [package])) WITHOUT ROWID;
CREATE TABLE [tags2](rowid INTEGER PRIMARY KEY, [tag] TEXT NOT NULL);
CREATE TABLE [tags2_map]([tag] INT64 NOT NULL, [package] INT64 NOT NULL,
  PRIMARY KEY([tag], [package])) WITHOUT ROWID;
"""

# (rowid, id, name, moniker, latest_version, norm_name, norm_publisher)
#
# Why each one is here:
#   1 7zip.7zip            a plain hit, and the non-GUID product code "7-zip"
#   2 Mozilla.Firefox      the base package of a family whose locale variant
#   3 Mozilla.Firefox.af   shares its moniker: the ordering rule's whole point
#   4 Python.Python.3.0    two members of a version family the index folds to
#   5 Python.Python.3.14   one norm_name and one norm_publisher
#   6 Valve.Steam          moniker `steam`, so an exact-moniker hit for "steam"
#   9 Codeusa.SteamCleaner a real catalogue package that also matches "steam",
#                          at a worse rank, and whose id sorts BEFORE
#                          Valve.Steam alphabetically. That is what makes rank
#                          precedence testable: if rank stopped outranking the
#                          tiebreak, this would come first.
#   7 Notepad++.Notepad++  a GUID product code and a non-GUID one at once
#   8 Obsidian.Obsidian    a GUID with no braces, and no upgrade code
PACKAGES = [
    (1, "7zip.7zip", "7-Zip", "7zip", "26.03", "7zip", "igorpavlov"),
    (2, "Mozilla.Firefox", "Mozilla Firefox (en-US)", "firefox", "156.0", "mozillafirefox", "mozilla"),
    (3, "Mozilla.Firefox.af", "Mozilla Firefox (af)", "firefox", "156.0", "mozillafirefox", "mozilla"),
    (4, "Python.Python.3.0", "Python 3.0", "python3", "3.0.1", "python", "pythonsoftwarefoundation"),
    (5, "Python.Python.3.14", "Python 3.14", "python3", "3.14.2", "python", "pythonsoftwarefoundation"),
    (6, "Valve.Steam", "Steam", "steam", "2.10.91.91", "steam", "valve"),
    (7, "Notepad++.Notepad++", "Notepad++", "notepad++", "8.9.8", "notepad", "donhonotepad"),
    (8, "Obsidian.Obsidian", "Obsidian", "obsidian", "1.13.7", "obsidian", "obsidian"),
    (9, "Codeusa.SteamCleaner", "SteamCleaner", "steamcleaner", "2.4", "steamcleaner", "codeusa"),
]

PRODUCT_CODES = [
    ("7-zip", 1),
    ("{23170f69-40c1-2701-2603-000001000000}", 1),
    ("mozilla firefox 156.0 (x64 en-us)", 2),
    ("{6b1c1b1e-0000-0000-0000-000000000314}", 5),
    ("notepad++", 7),
    ("{224c0e17-fb79-4ae2-9a47-5556fcef39c4}", 7),
    ("bd400747-f0c1-5638-a859-982036102edf", 8),
]

UPGRADE_CODES = [("{a1b2c3d4-0000-0000-0000-00000000f00d}", 7)]

TAGS = [(1, "archive"), (2, "browser"), (3, "editor")]
TAGS_MAP = [(1, 1), (2, 2), (2, 3), (3, 7), (3, 8)]


def main() -> None:
    OUT.parent.mkdir(parents=True, exist_ok=True)
    tmp = OUT.parent / "index.db"
    tmp.unlink(missing_ok=True)
    db = sqlite3.connect(tmp)
    db.executescript(SCHEMA)
    db.executemany(
        "INSERT INTO metadata(name, value) VALUES (?, ?)",
        [
            ("databaseIdentifier", "{43EFAB06-C6EF-49E2-AB0F-7EF010DAD9D3}"),
            ("lastwritetime", "1789793494"),
            ("majorVersion", "2"),
            ("minorVersion", "0"),
            ("updateTrackingBase", "1789793433"),
        ],
    )
    for rowid, pid, name, moniker, version, norm_name, norm_pub in PACKAGES:
        db.execute(
            "INSERT INTO packages(rowid, id, name, moniker, latest_version) VALUES (?,?,?,?,?)",
            (rowid, pid, name, moniker, version),
        )
        db.execute("INSERT INTO norm_names2(norm_name, package) VALUES (?,?)", (norm_name, rowid))
        db.execute(
            "INSERT INTO norm_publishers2(norm_publisher, package) VALUES (?,?)",
            (norm_pub, rowid),
        )
    db.executemany("INSERT INTO productcodes2(productcode, package) VALUES (?,?)", PRODUCT_CODES)
    db.executemany("INSERT INTO upgradecodes2(upgradecode, package) VALUES (?,?)", UPGRADE_CODES)
    db.executemany("INSERT INTO tags2(rowid, tag) VALUES (?,?)", TAGS)
    db.executemany("INSERT INTO tags2_map(tag, package) VALUES (?,?)", TAGS_MAP)
    db.commit()
    db.close()

    with zipfile.ZipFile(OUT, "w", zipfile.ZIP_DEFLATED) as z:
        # The real package carries assets and a manifest beside the database.
        # One of each is enough to prove the reader picks the right member.
        z.writestr("AppxManifest.xml", "<Package />")
        z.writestr("Assets/AppPackageStoreLogo.scale-100.png", b"\x89PNG\r\n\x1a\n")
        z.write(tmp, "Public/index.db")
    tmp.unlink()
    print(f"wrote {OUT} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
```

- [ ] **Step 3: Build the fixture**

Run: `python3 tools/make-winget-fixture.py`
Expected: `wrote .../source2.msix (NNNN bytes)`, a file of a few kilobytes.

- [ ] **Step 4: Write the failing tests**

Create `crates/brokey-core/src/sources/windows/winget/index.rs` holding only
this test module, so it fails to compile against functions that do not exist:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_bytes() -> Vec<u8> {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/windows/winget/source2.msix"
        );
        std::fs::read(path).expect("the fixture is checked in; run tools/make-winget-fixture.py")
    }

    /// The catalogue arrives as a zip with the database as one member among
    /// several. Picking it out is the whole of the unwrapping.
    #[test]
    fn the_database_comes_out_of_the_package() {
        let db = database_from_msix(&fixture_bytes()).unwrap();
        assert_eq!(&db[..15], b"SQLite format 3", "it is a SQLite file");
    }

    /// A zip that is not a catalogue says so rather than panicking.
    #[test]
    fn a_package_without_a_database_is_an_error() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            z.start_file::<_, ()>("AppxManifest.xml", Default::default())
                .unwrap();
            z.finish().unwrap();
        }
        let e = database_from_msix(buf.get_ref()).unwrap_err();
        assert!(e.message.contains("Public/index.db"), "{}", e.message);
    }

    /// Something that is not a zip at all.
    #[test]
    fn rubbish_is_an_error_not_a_panic() {
        let e = database_from_msix(b"not a zip").unwrap_err();
        assert!(e.message.contains("catalogue"), "{}", e.message);
    }

    /// The reader checks the schema it was built against rather than trusting
    /// the table names to still be there. `source.msix` answers 1 here and
    /// has entirely different tables.
    #[test]
    fn the_schema_version_is_read_from_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        std::fs::write(&path, database_from_msix(&fixture_bytes()).unwrap()).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(schema_major(&db).unwrap(), SCHEMA_MAJOR);
    }

    /// An older index is refused by name, with the version it turned out to
    /// be, because "it did not work" is not a sentence anyone can act on.
    #[test]
    fn an_older_schema_is_refused_with_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE metadata(name TEXT PRIMARY KEY, value TEXT);
             INSERT INTO metadata VALUES ('majorVersion','1'),('minorVersion','7');",
        )
        .unwrap();
        drop(db);
        let e = open(&path).unwrap_err();
        assert!(e.message.contains('1'), "names what it found: {}", e.message);
        assert!(e.message.contains('2'), "and what it wants: {}", e.message);
    }
}
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test -p brokey-core winget::index`
Expected: FAIL to compile, `cannot find function database_from_msix in this scope`.

- [ ] **Step 6: Write the implementation**

Put this above the test module in `index.rs`:

```rust
//! The winget catalogue: one file, fetched and cached by Brokey itself.
//!
//! Microsoft publishes the whole package index as `source2.msix` on a CDN. It
//! is a plain zip wrapping a SQLite database, so Brokey reads it directly
//! rather than asking `winget.exe` for anything. That is the decision
//! `sources/linux/pacman.rs` makes about the sync tarball, for the same
//! reasons: it is faster, it is not a human-readable format that can be
//! reformatted under us, and it works when the tool is missing. The last one
//! is why winget is searchable on a machine that has never had winget.
//!
//! Nothing here is gated. A zip is a zip and SQLite is SQLite on either
//! platform, so the tests run on Linux.

use crate::{Error, Result};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Where the whole catalogue comes from. `source.msix` beside it is the older
/// index: five times the bytes for the same 14,896 packages, in a schema this
/// reader does not know.
pub const CATALOGUE_URL: &str = "https://cdn.winget.microsoft.com/cache/source2.msix";

/// The member of the zip that matters. The rest is a manifest and store logos.
pub const DATABASE_MEMBER: &str = "Public/index.db";

/// The schema this reader was written against. `metadata` in the database
/// names its own, and the two have to agree before any table name is trusted.
pub const SCHEMA_MAJOR: i64 = 2;

/// The catalogue is rebuilt daily, so a day-old copy is a day-old catalogue,
/// which is what `winget source update` would have given anyway.
pub const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Unwrap `Public/index.db` from the package.
pub fn database_from_msix(bytes: &[u8]) -> Result<Vec<u8>> {
    let cursor = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor).map_err(|e| {
        Error::new(format!("The winget catalogue is not a readable package: {e}."))
    })?;
    let mut member = zip.by_name(DATABASE_MEMBER).map_err(|_| {
        Error::new(format!(
            "The winget catalogue has no {DATABASE_MEMBER} in it, so there is no index to read."
        ))
    })?;
    let mut out = Vec::with_capacity(member.size() as usize);
    member.read_to_end(&mut out).map_err(|e| {
        Error::new(format!("The winget catalogue could not be unpacked: {e}."))
    })?;
    Ok(out)
}

/// The schema version the database says it is.
pub fn schema_major(db: &rusqlite::Connection) -> Result<i64> {
    let text: String = db
        .query_row(
            "SELECT value FROM metadata WHERE name = 'majorVersion'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| Error::new(format!("The winget catalogue has no schema version: {e}.")))?;
    text.trim().parse::<i64>().map_err(|_| {
        Error::new(format!(
            "The winget catalogue's schema version is {text}, which is not a number."
        ))
    })
}

/// Open a catalogue that is already on disk, read-only, refusing a schema
/// this reader does not know.
pub fn open(path: &Path) -> Result<rusqlite::Connection> {
    let db = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| Error::new(format!("The winget catalogue could not be opened: {e}.")))?;
    let found = schema_major(&db)?;
    if found != SCHEMA_MAJOR {
        return Err(Error::new(format!(
            "The winget catalogue is schema version {found} and Brokey reads version \
             {SCHEMA_MAJOR}. Brokey needs an update before it can search winget."
        )));
    }
    Ok(db)
}

/// Where the unwrapped database lives between runs.
pub fn cached_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("winget").join("index.db")
}
```

Create `crates/brokey-core/src/sources/windows/winget/mod.rs` holding only:

```rust
//! winget: the primary Windows source.

pub mod index;
```

Add to `crates/brokey-core/src/sources/windows/mod.rs`, after `pub mod arp;`:

```rust
pub mod winget;
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p brokey-core winget::index`
Expected: PASS, 5 tests.

- [ ] **Step 8: Check the whole workspace**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets && cargo fmt --all --check`
Expected: all green. The first build compiles SQLite and is slow.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock crates/brokey-core/Cargo.toml \
  crates/brokey-core/src/sources/windows/mod.rs \
  crates/brokey-core/src/sources/windows/winget/ \
  crates/brokey-core/tests/fixtures/windows/winget/ \
  tools/make-winget-fixture.py
git commit -m "winget: unwrap the catalogue and check its schema"
```

---

### Task 2: Searching the catalogue

**Files:**
- Create: `crates/brokey-core/src/sources/windows/winget/query.rs`
- Modify: `crates/brokey-core/src/sources/windows/winget/mod.rs`

**Interfaces:**
- Consumes: `index::open`, `index::database_from_msix` from Task 1.
- Produces:
  - `pub struct Row { pub id: String, pub name: String, pub moniker: Option<String>, pub latest_version: String, pub publisher: Option<String> }`
  - `pub fn normalise(text: &str) -> String`
  - `pub fn search(db: &rusqlite::Connection, text: &str, limit: usize) -> crate::Result<Vec<Row>>`
  - `pub fn by_id(db: &rusqlite::Connection, id: &str) -> crate::Result<Option<Row>>`
  - `pub fn count(db: &rusqlite::Connection) -> crate::Result<i64>`

**The ordering rule, and why it is what it is.** Both halves were measured
against the live catalogue; both were wrong on the first attempt, and the
fixture pins each case.

Matches are ranked by *how* they matched, and ties are broken by the length of
the package id, shortest first, then by the id itself.

The id-length tiebreak is not decoration. `firefox` matches the moniker of
`Mozilla.Firefox` and of about a hundred locale builds, `Mozilla.Firefox.af`
through `Mozilla.Firefox.zu`, all at the same rank. Ordering by name length
puts `Mozilla Firefox (af)` above `Mozilla Firefox (en-US)` and the real
package never appears. Ordering by id length puts the base package first,
because every variant's id is its id plus a suffix.

The other half is what the rule deliberately does **not** try to do. Searching
`python` matches fifteen packages, `Python.Python.3.0` through
`Python.Python.3.14`, and no ordering of them is right, because they are not
fifteen answers. The index says so itself: all fifteen carry the `norm_names2`
row `python` and the `norm_publishers2` row `pythonsoftwarefoundation`. They
are one application with fifteen editions, and `group.rs` is what collapses
them, on the spec's second rung, normalised name plus normalised publisher.
That is why `Row` carries `publisher` and why Task 5 must put it on the
`Package`. Do not add a version-family heuristic here.

- [ ] **Step 1: Write the failing tests**

Create `query.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture, unwrapped into a temporary file and opened. Each test
    /// gets its own so nothing leaks between them.
    fn db() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/windows/winget/source2.msix"
        ))
        .unwrap();
        std::fs::write(&path, super::super::index::database_from_msix(&bytes).unwrap()).unwrap();
        let conn = super::super::index::open(&path).unwrap();
        (dir, conn)
    }

    fn ids(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.id.as_str()).collect()
    }

    /// An exact id beats everything.
    #[test]
    fn an_exact_id_comes_first() {
        let (_d, db) = db();
        let rows = search(&db, "Valve.Steam", 10).unwrap();
        assert_eq!(ids(&rows)[0], "Valve.Steam");
    }

    /// The moniker is what a person actually types.
    #[test]
    fn a_moniker_finds_the_package() {
        let (_d, db) = db();
        let rows = search(&db, "7zip", 10).unwrap();
        assert_eq!(ids(&rows)[0], "7zip.7zip");
    }

    /// A base package comes before its variants. The locale build shares the
    /// base package's moniker and matches at the same rank, so only the
    /// tiebreak separates them, and a winget id is `Publisher.Product` with
    /// an optional `.Variant` appended: the variant sorts straight after the
    /// base it came from. In the live catalogue all 102 packages with the
    /// moniker `firefox` are ids beginning `Mozilla.Firefox`.
    #[test]
    fn the_base_package_beats_its_locale_variant() {
        let (_d, db) = db();
        let rows = search(&db, "firefox", 10).unwrap();
        assert_eq!(
            ids(&rows),
            vec!["Mozilla.Firefox", "Mozilla.Firefox.af"],
            "the base package leads its own variant"
        );
    }

    /// Rank outranks the tiebreak. `Codeusa.SteamCleaner` sorts before
    /// `Valve.Steam` alphabetically and would lead on the tiebreak alone, but
    /// Steam matches its moniker exactly and SteamCleaner only contains the
    /// word, so Steam wins on rank. Swap the two `ORDER BY` terms and this
    /// fails, which is the point of it.
    #[test]
    fn a_better_match_beats_an_earlier_id() {
        let (_d, db) = db();
        let rows = search(&db, "steam", 10).unwrap();
        assert!(
            ids(&rows).contains(&"Codeusa.SteamCleaner"),
            "the worse match is in the results at all: {:?}",
            ids(&rows)
        );
        assert_eq!(
            ids(&rows)[0],
            "Valve.Steam",
            "and loses to the exact moniker despite sorting first: {:?}",
            ids(&rows)
        );
    }

    /// Case folds. Nobody types a package id with its capitals.
    #[test]
    fn matching_ignores_case() {
        let (_d, db) = db();
        assert_eq!(ids(&search(&db, "OBSIDIAN", 10).unwrap())[0], "Obsidian.Obsidian");
    }

    /// The version family is returned whole rather than ranked. Fifteen
    /// Pythons are one application with fifteen editions and `group.rs`
    /// joins them by normalised name and publisher, so the publisher has to
    /// come back with the row.
    #[test]
    fn a_version_family_comes_back_with_its_publisher() {
        let (_d, db) = db();
        let rows = search(&db, "python", 10).unwrap();
        let mut got = ids(&rows);
        got.sort();
        assert_eq!(got, vec!["Python.Python.3.0", "Python.Python.3.14"]);
        for r in &rows {
            assert_eq!(r.publisher.as_deref(), Some("pythonsoftwarefoundation"));
        }
    }

    /// A word nothing carries is an empty answer, not an error.
    #[test]
    fn no_match_is_an_empty_list() {
        let (_d, db) = db();
        assert!(search(&db, "zzzznothing", 10).unwrap().is_empty());
    }

    /// The limit is honoured, because the page asks for one.
    #[test]
    fn the_limit_is_respected() {
        let (_d, db) = db();
        assert_eq!(search(&db, "o", 3).unwrap().len(), 3);
    }

    /// Fetching one package by its exact id, for the detail page.
    #[test]
    fn one_package_by_id() {
        let (_d, db) = db();
        let row = by_id(&db, "Obsidian.Obsidian").unwrap().unwrap();
        assert_eq!(row.name, "Obsidian");
        assert_eq!(row.latest_version, "1.13.7");
        assert!(by_id(&db, "No.Such.Package").unwrap().is_none());
    }

    /// The status bar says how big the catalogue is.
    #[test]
    fn the_catalogue_can_be_counted() {
        let (_d, db) = db();
        assert_eq!(count(&db).unwrap(), 9);
    }

    /// Normalisation matches the index's own: letters and digits, folded.
    #[test]
    fn normalising_keeps_letters_and_digits_only() {
        assert_eq!(normalise("Mozilla Firefox (en-US)"), "mozillafirefoxenus");
        assert_eq!(normalise("Notepad++"), "notepad");
        assert_eq!(normalise("  7-Zip  "), "7zip");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p brokey-core winget::query`
Expected: FAIL to compile, `cannot find function search in this scope`.

- [ ] **Step 3: Write the implementation**

Above the test module in `query.rs`:

```rust
//! Every statement Brokey runs against the winget catalogue, and the row it
//! maps them to.
//!
//! Pure: it takes a connection and gives back rows. The connection comes from
//! [`super::index`], and the tests build one from the checked-in fixture, so
//! all of this runs on Linux.

use crate::{Error, Result};

/// One package as the index knows it. The index is a search index and has no
/// description, homepage, licence or size column; those come from the
/// manifest, and fetching manifests is a later plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub moniker: Option<String>,
    pub latest_version: String,
    /// From `norm_publishers2`, already normalised by the index. `group.rs`
    /// joins a version family on this plus the normalised name, so it has to
    /// travel with the row.
    pub publisher: Option<String>,
}

/// The index's own normalisation: letters and digits, folded to lower case.
/// `Mozilla Firefox (en-US)` is stored as `mozillafirefoxenus`.
pub fn normalise(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Matches, best first.
///
/// The rank says how it matched, and ties break on the id. That is enough to
/// put a base package above its variants, because a winget id is
/// `Publisher.Product` with an optional `.Variant` on the end, so a variant
/// sorts immediately after the base it came from. All 102 packages in the
/// live catalogue whose moniker is `firefox` are ids beginning
/// `Mozilla.Firefox`, and `Mozilla.Firefox` sorts first among them.
///
/// An earlier draft broke ties on the length of the id first. It was dropped:
/// it changes nothing for the case it was written for, and in the 28 real
/// monikers where length and alphabet disagree it prefers whichever vendor
/// has the shorter name, which is not a measure of anything. For moniker
/// `dev-cpp` it would pick `Orwell.Dev-C++`, the abandoned fork, over
/// `Embarcadero.Dev-C++`, the maintained one.
const SEARCH_SQL: &str = "
SELECT p.id, p.name, p.moniker, p.latest_version, np.norm_publisher, MIN(r.rank) AS rank
FROM packages p
JOIN (
    SELECT rowid AS pkg, 0 AS rank FROM packages WHERE lower(id) = :exact
    UNION ALL SELECT rowid, 1 FROM packages WHERE lower(moniker) = :exact
    UNION ALL SELECT rowid, 2 FROM packages WHERE lower(name) = :exact
    UNION ALL SELECT package, 3 FROM norm_names2 WHERE norm_name = :norm
    UNION ALL SELECT rowid, 4 FROM packages WHERE lower(name) LIKE :prefix
    UNION ALL SELECT rowid, 5 FROM packages WHERE lower(id) LIKE :contains
    UNION ALL SELECT rowid, 6 FROM packages WHERE lower(name) LIKE :contains
) r ON r.pkg = p.rowid
LEFT JOIN norm_publishers2 np ON np.package = p.rowid
GROUP BY p.rowid
ORDER BY rank, p.id
LIMIT :limit
";

const BY_ID_SQL: &str = "
SELECT p.id, p.name, p.moniker, p.latest_version, np.norm_publisher
FROM packages p
LEFT JOIN norm_publishers2 np ON np.package = p.rowid
WHERE p.id = :id
LIMIT 1
";

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        id: r.get(0)?,
        name: r.get(1)?,
        moniker: r.get(2)?,
        latest_version: r.get(3)?,
        publisher: r.get(4)?,
    })
}

fn failed(what: &str, e: rusqlite::Error) -> Error {
    Error::new(format!("The winget catalogue could not be {what}: {e}."))
}

pub fn search(db: &rusqlite::Connection, text: &str, limit: usize) -> Result<Vec<Row>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let lower = text.to_lowercase();
    let mut stmt = db.prepare_cached(SEARCH_SQL).map_err(|e| failed("read", e))?;
    let rows = stmt
        .query_map(
            rusqlite::named_params! {
                ":exact": &lower,
                ":norm": normalise(text),
                ":prefix": format!("{lower}%"),
                ":contains": format!("%{lower}%"),
                ":limit": limit as i64,
            },
            row_from,
        )
        .map_err(|e| failed("searched", e))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| failed("searched", e))
}

pub fn by_id(db: &rusqlite::Connection, id: &str) -> Result<Option<Row>> {
    let mut stmt = db.prepare_cached(BY_ID_SQL).map_err(|e| failed("read", e))?;
    let mut rows = stmt
        .query_map(rusqlite::named_params! { ":id": id }, row_from)
        .map_err(|e| failed("read", e))?;
    match rows.next() {
        None => Ok(None),
        Some(r) => r.map(Some).map_err(|e| failed("read", e)),
    }
}

pub fn count(db: &rusqlite::Connection) -> Result<i64> {
    db.query_row("SELECT COUNT(*) FROM packages", [], |r| r.get(0))
        .map_err(|e| failed("counted", e))
}
```

Add `pub mod query;` to `winget/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p brokey-core winget::query`
Expected: PASS, 11 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/brokey-core/src/sources/windows/winget/
git commit -m "winget: search the catalogue, base package before its variants"
```

---

### Task 3: Comparing two Windows versions

**Files:**
- Create: `crates/brokey-core/src/sources/windows/winget/version.rs`
- Modify: `crates/brokey-core/src/sources/windows/winget/mod.rs`

**Interfaces:**
- Produces: `pub fn newer(installed: &str, candidate: &str) -> bool`, and
  `pub fn cmp(a: &str, b: &str) -> std::cmp::Ordering`.

**Why not `vercmp.rs`.** The workspace already has a version comparator, but it
is pacman's, complete with epochs and pacman's own rules about release
suffixes, and it is tested against a table generated by the real `vercmp`.
Windows versions are not pacman versions. Feeding `26.02-v1.5.7-R2` to a
comparator written for `1:2.3.4-5` gives an answer that is wrong in a way
nobody can predict. This is fifty lines and a table of cases.

- [ ] **Step 1: Write the failing tests**

Create `version.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    /// The whole of the behaviour, as a table. Each row is a real shape seen
    /// in the catalogue or the registry.
    #[test]
    fn versions_compare_segment_by_segment() {
        let cases: &[(&str, &str, Ordering)] = &[
            // Numeric segments compare as numbers, which is the entire point:
            // as text, "10" sorts before "7" and every update is missed.
            ("1.13.7", "1.13.10", Ordering::Less),
            ("2.9.99.99", "2.10.91.91", Ordering::Less),
            ("3.0.1", "3.14.2", Ordering::Less),
            // Equal is equal, including with different padding.
            ("156.0", "156.0", Ordering::Equal),
            ("1.02", "1.2", Ordering::Equal),
            // A missing segment is lower, so 1.0 is older than 1.0.1.
            ("1.0", "1.0.1", Ordering::Less),
            // Real catalogue shapes with text in them. Note this one is
            // settled by 2 against 3 at the second segment and never reaches
            // the text, which is why the two rows after it exist.
            ("26.02-v1.5.7-R2", "26.03", Ordering::Less),
            // A release beats a qualifier when they meet at the same
            // position. Every other row in this table diverges on a pair of
            // numbers first, so without these two the rule is never run.
            ("1.0.0-rc1", "1.0.0.1", Ordering::Less),
            ("26.02-v1", "26.02-2", Ordering::Less),
            // A string with no recognisable segment at all sorts below one
            // that has any, the same way a missing segment does.
            ("...", "1.0", Ordering::Less),
            ("8.9.8", "8.9.8", Ordering::Equal),
            // A version that is only text falls back to comparing text.
            ("unknown", "unknown", Ordering::Equal),
        ];
        for (a, b, want) in cases {
            assert_eq!(cmp(a, b), *want, "{a} against {b}");
            assert_eq!(cmp(b, a), want.reverse(), "{b} against {a}");
        }
    }

    /// The question the updates list actually asks.
    #[test]
    fn newer_is_true_only_when_it_is_really_newer() {
        assert!(newer("1.13.7", "1.13.10"));
        assert!(!newer("1.13.10", "1.13.7"));
        assert!(!newer("156.0", "156.0"));
    }

    /// An empty or absent version never produces a phantom update. The
    /// registry leaves DisplayVersion off often enough that this matters.
    #[test]
    fn an_empty_version_is_never_newer() {
        assert!(!newer("", "1.0"));
        assert!(!newer("1.0", ""));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p brokey-core winget::version`
Expected: FAIL to compile, `cannot find function cmp in this scope`.

- [ ] **Step 3: Write the implementation**

```rust
//! Comparing two Windows version strings.
//!
//! Not `crate::vercmp`: that one is pacman's, with epochs and pacman's rules
//! about release suffixes, tested against the real `vercmp`. A Windows
//! version is a dotted number that sometimes has text stuck to it, and
//! feeding `26.02-v1.5.7-R2` to a comparator written for `1:2.3.4-5` gives an
//! answer nobody can predict.
//!
//! The rule: split both into runs of digits and runs of everything else,
//! compare pairwise, digits as numbers and text as text, and treat a missing
//! segment as lower than any present one.

use std::cmp::Ordering;

#[derive(PartialEq, Eq)]
enum Part<'a> {
    Number(u64),
    Text(&'a str),
}

fn parts(v: &str) -> Vec<Part<'_>> {
    let mut out = Vec::new();
    let bytes = v.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Separators carry no ordering of their own: 1.2 and 1-2 are the
        // same two segments.
        if !bytes[i].is_ascii_alphanumeric() {
            i += 1;
            continue;
        }
        let digit = bytes[i].is_ascii_digit();
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_alphanumeric() && bytes[i].is_ascii_digit() == digit {
            i += 1;
        }
        let slice = &v[start..i];
        out.push(if digit {
            // A segment longer than u64 is not a version anyone ships; if
            // one turns up, keep it as text rather than panicking. Note what
            // that means when the other side has an ordinary number at the
            // same position: the rule below makes the number win, so an
            // oversized segment loses to a smaller one. Nothing real reaches
            // this, and losing beats panicking.
            match slice.parse::<u64>() {
                Ok(n) => Part::Number(n),
                Err(_) => Part::Text(slice),
            }
        } else {
            Part::Text(slice)
        });
    }
    out
}

/// Order two versions.
pub fn cmp(a: &str, b: &str) -> Ordering {
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let ord = match (pa.get(i), pb.get(i)) {
            (None, None) => Ordering::Equal,
            // A shorter version is older: 1.0 before 1.0.1.
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(Part::Number(x)), Some(Part::Number(y))) => x.cmp(y),
            (Some(Part::Text(x)), Some(Part::Text(y))) => x.cmp(y),
            // A number is a release, text beside it is a qualifier, and a
            // release beats a qualifier: 1.0.0.1 is newer than 1.0.0-rc1.
            // This arm only fires when the two meet at the same position; a
            // version that merely has text somewhere later, like
            // 26.02-v1.5.7-R2, is settled by its numbers long before.
            (Some(Part::Number(_)), Some(Part::Text(_))) => Ordering::Greater,
            (Some(Part::Text(_)), Some(Part::Number(_))) => Ordering::Less,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Whether `candidate` is something the updates list should offer. An empty
/// version on either side answers `false`: the registry leaves
/// `DisplayVersion` off often enough that guessing would invent updates.
pub fn newer(installed: &str, candidate: &str) -> bool {
    if installed.trim().is_empty() || candidate.trim().is_empty() {
        return false;
    }
    cmp(installed, candidate) == Ordering::Less
}
```

Add `pub mod version;` to `winget/mod.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p brokey-core winget::version`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/brokey-core/src/sources/windows/winget/
git commit -m "winget: compare Windows versions as numbers, not as text"
```

---

### Task 4: The source, its status, and setting winget up

**Files:**
- Modify: `crates/brokey-core/src/sources/windows/winget/mod.rs`
- Modify: `Cargo.toml`, `crates/brokey-core/Cargo.toml` (adds `sha2`)

**Interfaces:**
- Consumes: `index::*`, `query::count` from Tasks 1 and 2.
- Produces:
  - `pub struct Winget` with `pub fn new(client: std::sync::Arc<crate::http::Client>) -> Winget`
  - `pub fn winget_exe() -> Option<std::path::PathBuf>` (`#[cfg(windows)]`)
  - `pub struct Bootstrap { pub url: String, pub sha256: String, pub version: String }`
  - `pub fn bootstrap_steps(b: &Bootstrap, into: &std::path::Path) -> Vec<Step>`
  - The `impl Source for Winget` block, with `status()` and `setup()` filled in
    and every other method a `todo!()` that Tasks 5 to 7 replace.

**How verification works, and why it is two steps.** The spec adds one
invariant for Windows: nothing downloaded is run before it is verified. On
Linux this comes free, because setting Flatpak up is `pacman -S flatpak` and
the distribution's signature check is the verification. Windows has no such
guarantee, and the documented install for winget is a bundle fetched over
HTTPS.

Microsoft publishes a `.txt` beside
`Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle` in every `winget-cli`
release, holding that bundle's SHA-256. So `setup()` returns three steps:
download, verify, install. Verification is its own step whose command exits
non-zero on a mismatch, and the Runner stops a plan at the first failing step,
so the install can never run against a bundle that did not match. A source
still runs nothing: these are three `Command`s like any other.

- [ ] **Step 1: Add the hashing dependency**

In `Cargo.toml` under `[workspace.dependencies]`:

```toml
# Verifying the App Installer bundle against the SHA-256 Microsoft publishes
# beside it. The spec's one Windows invariant: nothing downloaded is run
# before it is verified.
sha2 = "0.10"
```

In `crates/brokey-core/Cargo.toml`: `sha2 = { workspace = true }`.

- [ ] **Step 2: Write the failing tests**

Add to `winget/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn bootstrap() -> Bootstrap {
        Bootstrap {
            url: "https://github.com/microsoft/winget-cli/releases/download/v1.29.290/\
                  Microsoft.DesktopAppInstaller_8wekyb3d8bbwe.msixbundle"
                .to_string(),
            sha256: "ab".repeat(32),
            version: "1.29.290".to_string(),
        }
    }

    /// Three steps, in the only order that is safe.
    #[test]
    fn setting_winget_up_downloads_then_verifies_then_installs() {
        let steps = bootstrap_steps(&bootstrap(), std::path::Path::new("C:/tmp"));
        assert_eq!(steps.len(), 3);
        assert!(steps[0].title.starts_with("Downloading"), "{}", steps[0].title);
        assert!(steps[1].title.starts_with("Checking"), "{}", steps[1].title);
        assert!(steps[2].title.starts_with("Installing"), "{}", steps[2].title);
    }

    /// The published hash reaches the command that checks it. Without this
    /// the verify step is decoration.
    #[test]
    fn the_verify_step_carries_the_published_hash() {
        let b = bootstrap();
        let steps = bootstrap_steps(&b, std::path::Path::new("C:/tmp"));
        let joined = steps[1].command.args.join(" ");
        assert!(joined.contains(&b.sha256), "the hash is in the command");
    }

    /// Nothing about setting winget up needs Administrator. This is what
    /// makes per-user-first worth having, and a regression here costs the
    /// user a UAC prompt they were promised they would not see.
    #[test]
    fn setting_winget_up_never_elevates() {
        for s in bootstrap_steps(&bootstrap(), std::path::Path::new("C:/tmp")) {
            assert!(!s.needs_root, "{} elevates", s.title);
        }
    }

    /// Every step says winget, so the activity panel attributes them.
    #[test]
    fn the_steps_belong_to_winget() {
        for s in bootstrap_steps(&bootstrap(), std::path::Path::new("C:/tmp")) {
            assert_eq!(s.source, SourceKind::Winget);
        }
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p brokey-core winget::tests`
Expected: FAIL to compile, `cannot find function bootstrap_steps`.

- [ ] **Step 4: Write the implementation**

Replace the contents of `winget/mod.rs` above the tests with:

```rust
//! winget: the primary Windows source.
//!
//! Search comes from the catalogue in [`index`], which Brokey fetches itself,
//! so this source is searchable on a machine that has never had winget. The
//! operations are `winget.exe` steps, which need the tool, so `available`
//! tracks the tool and `searchable` tracks the catalogue.

pub mod index;
pub mod query;
pub mod version;

use crate::model::{Command, SourceKind, SourceSetup, SourceStatus, Step};
use crate::{Result, Setup, Source};
use std::path::Path;
use std::sync::Arc;

/// Where the App Installer bundle and its hash come from. The release is
/// looked up at the moment it is needed rather than pinned, because pinning
/// a version means shipping a Brokey that installs an old winget forever.
pub const WINGET_CLI_LATEST: &str =
    "https://api.github.com/repos/microsoft/winget-cli/releases/latest";

pub const NOT_INSTALLED: &str =
    "winget is not installed, so nothing can be installed or removed through it. \
     Brokey still searches winget's catalogue, which it reads itself.";
pub const NO_BOOTSTRAP: &str =
    "winget is not installed, and the App Installer release could not be reached, \
     so Brokey cannot set it up just now. Brokey still searches winget's catalogue.";
pub const SETUP_LABEL: &str = "Install winget";
pub const SETUP_SENTENCE: &str =
    "The App Installer package is downloaded from Microsoft, checked against the \
     hash Microsoft publishes with it, and installed for you alone. No \
     Administrator permission is needed.";

/// What a `winget-cli` release says about its bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bootstrap {
    pub url: String,
    /// The SHA-256 Microsoft publishes in a `.txt` beside the bundle.
    pub sha256: String,
    pub version: String,
}

pub struct Winget {
    client: Arc<crate::http::Client>,
}

impl Winget {
    pub fn new(client: Arc<crate::http::Client>) -> Winget {
        Winget { client }
    }
}

/// Download, check, install. Three steps because the check has to be able to
/// stop the plan: the Runner abandons a plan at the first failing step, so an
/// install can never run against a bundle whose hash did not match.
pub fn bootstrap_steps(b: &Bootstrap, into: &Path) -> Vec<Step> {
    let file = into.join("Microsoft.DesktopAppInstaller.msixbundle");
    let file = file.display().to_string();
    let step = |title: String, program: &str, args: Vec<String>, weight: u32| Step {
        source: SourceKind::Winget,
        title,
        command: Command {
            program: program.to_string(),
            args,
            env: Vec::new(),
            cwd: None,
        },
        // Nothing here elevates. App Installer registers for one user.
        needs_root: false,
        weight,
    };
    vec![
        step(
            format!("Downloading App Installer {}", b.version),
            "curl.exe",
            vec![
                "-L".to_string(),
                "--fail".to_string(),
                "--create-dirs".to_string(),
                "-o".to_string(),
                file.clone(),
                b.url.clone(),
            ],
            8,
        ),
        step(
            "Checking what was downloaded".to_string(),
            "powershell.exe",
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!(
                    "if ((Get-FileHash -Algorithm SHA256 -LiteralPath '{file}').Hash -ne '{}') \
                     {{ Write-Error 'The App Installer package did not match the hash Microsoft \
                     publishes for it, so it was not installed.'; exit 1 }}",
                    b.sha256.to_uppercase()
                ),
            ],
            1,
        ),
        step(
            format!("Installing App Installer {}", b.version),
            "powershell.exe",
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                format!("Add-AppxPackage -LiteralPath '{file}'"),
            ],
            4,
        ),
    ]
}

/// Where `winget.exe` is, if it is anywhere.
#[cfg(windows)]
pub fn winget_exe() -> Option<std::path::PathBuf> {
    crate::system::windows::which("winget")
}

impl Source for Winget {
    fn kind(&self) -> SourceKind {
        SourceKind::Winget
    }

    fn status(&self) -> SourceStatus {
        let kind = SourceKind::Winget;
        #[cfg(windows)]
        let tool = winget_exe();
        #[cfg(not(windows))]
        let tool: Option<std::path::PathBuf> = None;

        if tool.is_none() {
            return SourceStatus {
                kind,
                available: false,
                reason: Some(NOT_INSTALLED.to_string()),
                detail: None,
                // The catalogue is Brokey's own file, so search works either way.
                searchable: true,
                // What setting winget up would do. Whether Microsoft's release
                // is reachable is deliberately not asked here: `status()` runs
                // every time the page draws a source list, and two network
                // requests per draw to prove a remedy will work is a cost
                // nobody agreed to. `setup()` finds out, once, when the user
                // actually asks for it.
                setup: Some(SourceSetup {
                    label: SETUP_LABEL.to_string(),
                    sentence: SETUP_SENTENCE.to_string(),
                }),
            };
        }
        let detail = self
            .catalogue()
            .ok()
            .and_then(|db| query::count(&db).ok())
            .map(|n| format!("{n} packages"));
        SourceStatus {
            kind,
            available: true,
            reason: None,
            detail,
            searchable: true,
            setup: None,
        }
    }

    fn setup(&self) -> Option<Setup> {
        #[cfg(windows)]
        if winget_exe().is_some() {
            return None;
        }
        let b = self.bootstrap().ok()?;
        Some(Setup {
            ops: Vec::new(),
            steps: bootstrap_steps(&b, &self.client.download_dir()),
            notice: SETUP_SENTENCE.to_string(),
        })
    }

    fn search(&self, _query: &crate::Query) -> Result<Vec<crate::model::Package>> {
        todo!("Task 5")
    }

    fn installed(&self) -> Result<Vec<crate::model::Package>> {
        todo!("Task 6")
    }

    fn updates(&self) -> Result<Vec<crate::model::Update>> {
        todo!("Task 6")
    }

    fn details(&self, _id: &str) -> Result<crate::model::Package> {
        todo!("Task 5")
    }

    fn plan(&self, _op: &crate::model::Op) -> Result<Vec<Step>> {
        todo!("Task 7")
    }
}
```

And the two private helpers, in the same file:

```rust
impl Winget {
    /// The catalogue, downloaded if the cached copy is missing or stale.
    fn catalogue(&self) -> Result<rusqlite::Connection> {
        let path = index::cached_path(&self.client.download_dir());
        let stale = match std::fs::metadata(&path) {
            Err(_) => true,
            Ok(m) => m
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|age| age > index::MAX_AGE),
        };
        if stale {
            let bytes = self.client.get_bytes(index::CATALOGUE_URL)?;
            let db = index::database_from_msix(&bytes)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, db).map_err(|e| {
                crate::Error::new(format!("The winget catalogue could not be saved: {e}."))
            })?;
        }
        index::open(&path)
    }

    /// The newest App Installer release, and the hash published with it.
    fn bootstrap(&self) -> Result<Bootstrap> {
        #[derive(serde::Deserialize)]
        struct Release {
            tag_name: String,
            assets: Vec<Asset>,
        }
        #[derive(serde::Deserialize)]
        struct Asset {
            name: String,
            browser_download_url: String,
        }
        let release: Release = self.client.get_json(WINGET_CLI_LATEST, &[])?;
        let bundle = release
            .assets
            .iter()
            .find(|a| a.name.ends_with(".msixbundle"))
            .ok_or_else(|| {
                crate::Error::new(
                    "The App Installer release has no package in it, so winget cannot be set up."
                        .to_string(),
                )
            })?;
        let hash_asset = release
            .assets
            .iter()
            .find(|a| a.name.ends_with(".txt") && a.name.contains("DesktopAppInstaller"))
            .ok_or_else(|| {
                crate::Error::new(
                    "The App Installer release publishes no hash for its package, so Brokey \
                     will not install it."
                        .to_string(),
                )
            })?;
        let sha256 = self
            .client
            .get_text(&hash_asset.browser_download_url)?
            .trim()
            .to_string();
        Ok(Bootstrap {
            url: bundle.browser_download_url.clone(),
            sha256,
            version: release.tag_name.trim_start_matches('v').to_string(),
        })
    }
}
```

**Implementer note, second.** `status()` advertises the Install winget button
without first proving Microsoft's release is reachable, and `setup()` returns
`None` when it is not. This was checked before you were dispatched, so you do
not need to: `transaction/plan.rs:87` turns a `None` from `setup()` into
`Err(cannot_set_up(kind))`, so the button reports an error rather than doing
nothing. The sentence it produces is the generic "winget cannot be set up on
this system by Brokey", which is not quite right for a machine that is merely
offline. That is a known and accepted wrinkle, recorded for the final review.
Do not widen this task to fix it, and do not change `plan.rs` or the `Source`
trait here.

**Implementer note.** `crate::system::windows::which` is the function Task 4 of
the Windows foundations plan added; check its exact name and signature before
calling it, and adjust this line rather than adding a second one. If the
signature is `which_in(name, path, pathext)`, use the wrapper beside it that
reads the real environment.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p brokey-core winget`
Expected: PASS. The `todo!()` methods are not called by any test yet.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock crates/brokey-core/Cargo.toml \
  crates/brokey-core/src/sources/windows/winget/
git commit -m "winget: say whether it is here, and offer to set it up"
```

---

### Task 5: Search results and the detail page

**Files:**
- Modify: `crates/brokey-core/src/sources/windows/winget/mod.rs`
- Modify: `crates/brokey-core/src/sources/windows/winget/query.rs`

**Interfaces:**
- Consumes: `query::Row`, `query::search`, `query::by_id`.
- Produces: `pub fn to_package(row: &query::Row) -> crate::model::Package`,
  and the filled-in `search()` and `details()`.

- [ ] **Step 1: Write the failing tests**

Add to `query.rs`'s test module:

```rust
/// The publisher reaches the Package. `group.rs` joins a version family on
/// normalised name plus normalised publisher, so without this the fifteen
/// Pythons in the real catalogue stay fifteen rows on the page forever.
#[test]
fn the_package_carries_the_publisher_the_grouper_needs() {
    let (_d, db) = db();
    let row = by_id(&db, "Python.Python.3.14").unwrap().unwrap();
    let p = super::super::to_package(&row);
    assert_eq!(p.developer.as_deref(), Some("pythonsoftwarefoundation"));
    assert_eq!(p.source, SourceKind::Winget);
    assert_eq!(p.id, "Python.Python.3.14");
    assert_eq!(p.version.as_deref(), Some("3.14.2"));
}

/// The index has no description, homepage, licence or size, and the source
/// says so by leaving them empty rather than inventing them. The metadata
/// ladder that fetches manifests is a later plan.
#[test]
fn fields_the_index_does_not_have_are_empty() {
    let (_d, db) = db();
    let p = super::super::to_package(&by_id(&db, "7zip.7zip").unwrap().unwrap());
    assert!(p.description.is_none());
    assert!(p.homepage.is_none());
    assert!(p.licence.is_none());
    assert!(p.download_size.is_none());
    assert!(p.screenshots.is_empty());
}

/// The moniker is what a person types, so it earns a row on the detail page.
#[test]
fn the_moniker_is_shown_as_a_fact() {
    let (_d, db) = db();
    let p = super::super::to_package(&by_id(&db, "Valve.Steam").unwrap().unwrap());
    assert!(
        p.facts.iter().any(|(k, v)| k == "Moniker" && v == "steam"),
        "{:?}",
        p.facts
    );
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p brokey-core winget::query`
Expected: FAIL, `cannot find function to_package`.

- [ ] **Step 3: Implement `to_package` in `winget/mod.rs`**

```rust
/// A catalogue row as the page's `Package`.
///
/// The index is a search index: it has an id, a name, a moniker, a version
/// and a publisher, and nothing else. Every other field stays `None` rather
/// than being guessed at, which is what `Package` already expects of a source
/// that does not know them. Descriptions, homepages and icons arrive with the
/// metadata ladder in a later plan.
pub fn to_package(row: &query::Row) -> crate::model::Package {
    let mut facts = Vec::new();
    facts.push(("Package id".to_string(), row.id.clone()));
    if let Some(m) = &row.moniker
        && !m.trim().is_empty()
    {
        facts.push(("Moniker".to_string(), m.clone()));
    }
    crate::model::Package {
        source: SourceKind::Winget,
        id: row.id.clone(),
        name: row.name.clone(),
        kind: crate::model::PackageKind::App,
        summary: None,
        description: None,
        version: Some(row.latest_version.clone()),
        installed_version: None,
        installed: false,
        repo: Some("winget".to_string()),
        licence: None,
        homepage: None,
        // The grouper's second rung is normalised name plus normalised
        // publisher. The index has already normalised this one.
        developer: row.publisher.clone(),
        updated: None,
        download_size: None,
        installed_size: None,
        popularity: None,
        popularity_label: None,
        icon: None,
        screenshots: Vec::new(),
        categories: Vec::new(),
        appstream_id: None,
        out_of_date: false,
        sandboxed: false,
        facts,
    }
}
```

Fill in the two trait methods, replacing their `todo!()`:

```rust
fn search(&self, query: &crate::Query) -> Result<Vec<crate::model::Package>> {
    let db = self.catalogue()?;
    let rows = query::search(&db, &query.text, query.limit)?;
    Ok(rows.iter().map(to_package).collect())
}

fn details(&self, id: &str) -> Result<crate::model::Package> {
    let db = self.catalogue()?;
    let row = query::by_id(&db, id)?.ok_or_else(|| {
        crate::Error::from_source(
            SourceKind::Winget,
            format!("{id} is not in the winget catalogue."),
        )
    })?;
    Ok(to_package(&row))
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p brokey-core winget`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/brokey-core/src/sources/windows/winget/
git commit -m "winget: search results, with the publisher the grouper needs"
```

---

### Task 6: What is installed, and what has a newer version

**Files:**
- Modify: `crates/brokey-core/src/sources/windows/winget/mod.rs`
- Modify: `crates/brokey-core/src/sources/windows/winget/query.rs`

**Interfaces:**
- Consumes: `crate::sources::windows::arp::{RawEntry, is_application}`,
  `version::newer`.
- Produces:
  - `pub fn match_entry(db: &rusqlite::Connection, e: &RawEntry) -> crate::Result<Option<query::Row>>`
  - `pub fn installed_from(db: &rusqlite::Connection, entries: &[RawEntry]) -> crate::Result<Vec<Package>>`
  - `pub fn updates_from(db: &rusqlite::Connection, entries: &[RawEntry]) -> crate::Result<Vec<Update>>`
  - The filled-in `installed()` and `updates()`.

**What this is.** winget has no installed-package database of its own that
Brokey should read; what `winget list` does is join the uninstall registry
against the catalogue, which is why `productcodes2` exists at all. So winget's
`installed()` is exactly that join: every Add/Remove Programs entry that
matches a catalogue package, reported as a winget package with the installed
version filled in. The Add/Remove Programs source reports the same
applications from its own side, and `group.rs` joins the two into one
application with two editions, the winget one being the edition that can be
updated. That is the design, not a duplication.

Measured on the reference machine: 157 applications, 68 matched by product
code, 17 more by normalised name, 72 unmatched.

**The ladder, in order.** Each rung is a test.

1. The uninstall key name against `productcodes2`, folding case. This is the
   exact rung, and it is the only one that is exact.
2. The same against `upgradecodes2`.
3. The normalised `DisplayName` against `norm_names2`, **only when the
   normalised publisher also matches** `norm_publishers2`. Name alone joins
   `Python 3.0` to every other Python; name plus publisher is the spec's
   second rung and is what makes it safe.

- [ ] **Step 1: Write the failing tests**

Add to `query.rs`'s test module:

```rust
fn entry(key: &str, name: &str, version: &str, publisher: &str) -> crate::sources::windows::arp::RawEntry {
    crate::sources::windows::arp::RawEntry {
        hive: crate::sources::windows::arp::Hive::Machine,
        key_name: key.to_string(),
        display_name: Some(name.to_string()),
        display_version: Some(version.to_string()),
        publisher: Some(publisher.to_string()),
        install_location: None,
        uninstall_string: None,
        quiet_uninstall_string: None,
        windows_installer: None,
        display_icon: None,
        system_component: None,
        parent_key_name: None,
        release_type: None,
        estimated_size: None,
        url_info_about: None,
    }
}

/// The exact rung. The registry spells the GUID in capitals and the
/// catalogue stores it in lower case, so the join has to fold.
#[test]
fn an_uninstall_key_matches_its_product_code_whatever_the_case() {
    let (_d, db) = db();
    let e = entry("{23170F69-40C1-2701-2603-000001000000}", "7-Zip 26.03 (x64)", "26.03", "Igor Pavlov");
    let row = super::super::match_entry(&db, &e).unwrap().unwrap();
    assert_eq!(row.id, "7zip.7zip");
}

/// Not every product code is a GUID. The column holds whatever the key is
/// named, and `notepad++` is a real row in the real catalogue.
#[test]
fn a_product_code_that_is_not_a_guid_still_matches() {
    let (_d, db) = db();
    let e = entry("notepad++", "Notepad++ (64-bit x64)", "8.9.8", "Notepad++ Team");
    assert_eq!(super::super::match_entry(&db, &e).unwrap().unwrap().id, "Notepad++.Notepad++");
}

/// The upgrade code rung, for an entry whose product code changed between
/// versions but whose upgrade code did not.
#[test]
fn an_upgrade_code_matches_when_the_product_code_does_not() {
    let (_d, db) = db();
    let e = entry("{A1B2C3D4-0000-0000-0000-00000000F00D}", "Something Else", "1.0", "Nobody");
    assert_eq!(super::super::match_entry(&db, &e).unwrap().unwrap().id, "Notepad++.Notepad++");
}

/// The name rung fires only with the publisher beside it.
#[test]
fn a_name_matches_when_the_publisher_agrees() {
    let (_d, db) = db();
    let e = entry("Obsidian_is_not_a_code", "Obsidian", "1.13.0", "Obsidian");
    assert_eq!(super::super::match_entry(&db, &e).unwrap().unwrap().id, "Obsidian.Obsidian");
}

/// And not without it. This is the rung that would otherwise join every
/// Python to every other Python, and a wrong join offers an update that
/// replaces one application with a different one.
#[test]
fn a_name_alone_is_not_enough() {
    let (_d, db) = db();
    let e = entry("SomeKey", "Obsidian", "1.13.0", "A Different Company Entirely");
    assert!(super::super::match_entry(&db, &e).unwrap().is_none());
}

/// Something the catalogue has never heard of.
#[test]
fn an_unmatched_entry_is_none() {
    let (_d, db) = db();
    let e = entry("NothingLikeThis", "Bespoke Internal Tool", "4.2", "Our IT Department");
    assert!(super::super::match_entry(&db, &e).unwrap().is_none());
}

/// An older installed version against the catalogue's newest is an update,
/// and it names both ends.
#[test]
fn an_older_installed_version_is_an_update() {
    let (_d, db) = db();
    let e = entry("bd400747-f0c1-5638-a859-982036102edf", "Obsidian", "1.10.0", "Obsidian");
    let ups = super::super::updates_from(&db, std::slice::from_ref(&e)).unwrap();
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].from.as_deref(), Some("1.10.0"));
    assert_eq!(ups[0].to, "1.13.7");
}

/// The same version is not an update, and neither is a newer installed one.
#[test]
fn an_up_to_date_entry_is_not_an_update() {
    let (_d, db) = db();
    let same = entry("bd400747-f0c1-5638-a859-982036102edf", "Obsidian", "1.13.7", "Obsidian");
    assert!(super::super::updates_from(&db, std::slice::from_ref(&same)).unwrap().is_empty());
    let ahead = entry("bd400747-f0c1-5638-a859-982036102edf", "Obsidian", "2.0.0", "Obsidian");
    assert!(super::super::updates_from(&db, std::slice::from_ref(&ahead)).unwrap().is_empty());
}

/// Installed packages carry the version that is on the machine, not the
/// catalogue's, and say they are installed.
#[test]
fn installed_packages_report_the_installed_version() {
    let (_d, db) = db();
    let e = entry("7-zip", "7-Zip 26.00 (x64)", "26.00", "Igor Pavlov");
    let ps = super::super::installed_from(&db, std::slice::from_ref(&e)).unwrap();
    assert_eq!(ps.len(), 1);
    assert!(ps[0].installed);
    assert_eq!(ps[0].installed_version.as_deref(), Some("26.00"));
    assert_eq!(ps[0].version.as_deref(), Some("26.03"));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p brokey-core winget::query`
Expected: FAIL, `cannot find function match_entry`.

- [ ] **Step 3: Implement the join in `winget/mod.rs`**

```rust
const BY_CODE_SQL: &str = "
SELECT p.id, p.name, p.moniker, p.latest_version, np.norm_publisher
FROM packages p
LEFT JOIN norm_publishers2 np ON np.package = p.rowid
WHERE p.rowid = (SELECT package FROM productcodes2 WHERE productcode = :code LIMIT 1)
   OR p.rowid = (SELECT package FROM upgradecodes2 WHERE upgradecode = :code LIMIT 1)
LIMIT 1
";

const BY_NAME_AND_PUBLISHER_SQL: &str = "
SELECT p.id, p.name, p.moniker, p.latest_version, np.norm_publisher
FROM packages p
JOIN norm_names2 nn ON nn.package = p.rowid AND nn.norm_name = :name
JOIN norm_publishers2 np ON np.package = p.rowid AND np.norm_publisher = :publisher
ORDER BY length(p.id), p.id
LIMIT 1
";

/// The catalogue package an uninstall entry is, if it is one.
///
/// Three rungs, exact first. The product and upgrade code rungs are exact;
/// the name rung requires the publisher to agree as well, because a name on
/// its own joins `Python 3.0` to every other Python, and a wrong join offers
/// an update that replaces one application with a different one.
pub fn match_entry(
    db: &rusqlite::Connection,
    e: &crate::sources::windows::arp::RawEntry,
) -> Result<Option<query::Row>> {
    // Codes are stored lower-case in the catalogue and spelt however the
    // installer felt in the registry. Fold both sides.
    let code = e.key_name.trim().to_lowercase();
    if let Some(row) = query::one(db, BY_CODE_SQL, &[(":code", &code)])? {
        return Ok(Some(row));
    }
    let (Some(name), Some(publisher)) = (&e.display_name, &e.publisher) else {
        return Ok(None);
    };
    query::one(
        db,
        BY_NAME_AND_PUBLISHER_SQL,
        &[
            (":name", &query::normalise(name)),
            (":publisher", &query::normalise(publisher)),
        ],
    )
}

/// Everything the catalogue recognises on this machine.
pub fn installed_from(
    db: &rusqlite::Connection,
    entries: &[crate::sources::windows::arp::RawEntry],
) -> Result<Vec<crate::model::Package>> {
    let mut out = Vec::new();
    for e in entries.iter().filter(|e| crate::sources::windows::arp::is_application(e)) {
        if let Some(row) = match_entry(db, e)? {
            let mut p = to_package(&row);
            p.installed = true;
            p.installed_version = e.display_version.clone();
            out.push(p);
        }
    }
    Ok(out)
}

/// Those of them the catalogue has a newer version of.
pub fn updates_from(
    db: &rusqlite::Connection,
    entries: &[crate::sources::windows::arp::RawEntry],
) -> Result<Vec<crate::model::Update>> {
    let mut out = Vec::new();
    for e in entries.iter().filter(|e| crate::sources::windows::arp::is_application(e)) {
        let Some(row) = match_entry(db, e)? else {
            continue;
        };
        let have = e.display_version.clone().unwrap_or_default();
        if !version::newer(&have, &row.latest_version) {
            continue;
        }
        out.push(crate::model::Update {
            package: crate::model::PackageRef {
                source: SourceKind::Winget,
                id: row.id.clone(),
            },
            name: row.name.clone(),
            kind: crate::model::PackageKind::App,
            summary: None,
            icon: None,
            from: Some(have),
            to: row.latest_version.clone(),
            download_size: None,
            published: None,
            is_self: false,
        });
    }
    Ok(out)
}
```

Add this small helper to `query.rs`, beside `by_id`:

```rust
/// Run a statement that returns at most one row.
pub fn one(
    db: &rusqlite::Connection,
    sql: &str,
    params: &[(&str, &String)],
) -> Result<Option<Row>> {
    let mut stmt = db.prepare_cached(sql).map_err(|e| failed("read", e))?;
    let bound: Vec<(&str, &dyn rusqlite::ToSql)> =
        params.iter().map(|(k, v)| (*k, *v as &dyn rusqlite::ToSql)).collect();
    let mut rows = stmt
        .query_map(bound.as_slice(), row_from)
        .map_err(|e| failed("read", e))?;
    match rows.next() {
        None => Ok(None),
        Some(r) => r.map(Some).map_err(|e| failed("read", e)),
    }
}
```

Fill in the two trait methods:

```rust
fn installed(&self) -> Result<Vec<crate::model::Package>> {
    #[cfg(windows)]
    {
        let db = self.catalogue()?;
        installed_from(&db, &crate::sources::windows::arp::read())
    }
    #[cfg(not(windows))]
    Ok(Vec::new())
}

fn updates(&self) -> Result<Vec<crate::model::Update>> {
    #[cfg(windows)]
    {
        let db = self.catalogue()?;
        updates_from(&db, &crate::sources::windows::arp::read())
    }
    #[cfg(not(windows))]
    Ok(Vec::new())
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p brokey-core winget`
Expected: PASS, 9 new tests.

- [ ] **Step 5: Commit**

```bash
git add crates/brokey-core/src/sources/windows/winget/
git commit -m "winget: join the registry to the catalogue, exactly where it can"
```

---

### Task 7: Planning an install, an update and a removal

**Files:**
- Modify: `crates/brokey-core/src/sources/windows/winget/mod.rs`

**Interfaces:**
- Produces: `pub fn operation_step(kind: OpKind, id: &str) -> Step`,
  `pub enum OpKind { Install, Update, Remove }`, and the filled-in `plan()`.

**The flags, and why each one is there.** From the spec: `--silent
--accept-package-agreements --accept-source-agreements` throughout. The helper
runs with no terminal, so a prompt would hang the plan rather than ask
anybody anything. `--exact` because the id is exact and a near match would
install something the user did not choose. `--disable-interactivity` for the
same reason as `--silent`.

`--scope user` is **not** set unconditionally. A package whose manifest offers
only a machine-wide installer fails outright when told `--scope user`, which
turns "this one needs Administrator" into "this one is broken". Per-user-first
is the spec's rule, and the honest reading of it here is to let winget choose
and to mark the step as elevating, because winget itself prompts for elevation
when the manifest requires it. Setting the scope per package needs the
manifest, which is the metadata ladder, which is a later plan. Record it and
move on.

- [ ] **Step 1: Write the failing tests**

```rust
/// Every operation is non-interactive, because the helper has no terminal
/// and a prompt would hang the plan rather than ask anyone anything.
#[test]
fn every_operation_is_silent_and_pre_agreed() {
    for kind in [OpKind::Install, OpKind::Update, OpKind::Remove] {
        let s = operation_step(kind, "Valve.Steam");
        let args = s.command.args.join(" ");
        assert!(args.contains("--silent"), "{args}");
        assert!(args.contains("--disable-interactivity"), "{args}");
        assert!(args.contains("--accept-source-agreements"), "{args}");
        assert_eq!(s.command.program, "winget.exe");
    }
}

/// An install names the package exactly. A near match would install
/// something the user did not choose.
#[test]
fn an_install_is_exact_and_names_the_id() {
    let s = operation_step(OpKind::Install, "Valve.Steam");
    assert_eq!(s.command.args[0], "install");
    assert!(s.command.args.contains(&"--exact".to_string()));
    assert!(s.command.args.contains(&"Valve.Steam".to_string()));
    assert_eq!(s.title, "Installing Valve.Steam");
}

/// Uninstall takes no package agreement, because nothing is being agreed to.
#[test]
fn a_removal_does_not_accept_a_package_agreement() {
    let s = operation_step(OpKind::Remove, "Valve.Steam");
    assert_eq!(s.command.args[0], "uninstall");
    assert!(!s.command.args.contains(&"--accept-package-agreements".to_string()));
}

/// A plan for an operation this source has nothing to do with is empty, not
/// an error. The store asks every source about every operation.
#[test]
fn an_operation_for_another_source_plans_nothing() {
    let w = Winget::new(crate::http::Client::shared());
    let op = crate::model::Op::Install {
        package: crate::model::PackageRef {
            source: SourceKind::Flatpak,
            id: "org.videolan.VLC".to_string(),
        },
    };
    assert!(w.plan(&op).unwrap().is_empty());
}

/// Updating everything winget can update is one step, not one per package.
#[test]
fn update_all_is_a_single_step() {
    let w = Winget::new(crate::http::Client::shared());
    let steps = w
        .plan(&crate::model::Op::UpdateAll { source: SourceKind::Winget })
        .unwrap();
    assert_eq!(steps.len(), 1);
    assert!(steps[0].command.args.contains(&"--all".to_string()));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p brokey-core winget`
Expected: FAIL, `cannot find type OpKind`.

- [ ] **Step 3: Implement**

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Install,
    Update,
    Remove,
}

/// One `winget.exe` call.
///
/// `--silent` and `--disable-interactivity` because the helper runs with no
/// terminal: a prompt would hang the plan rather than ask anybody anything.
/// `--exact` because the id is exact and a near match would install something
/// the user did not choose.
pub fn operation_step(kind: OpKind, id: &str) -> Step {
    let (verb, title) = match kind {
        OpKind::Install => ("install", format!("Installing {id}")),
        OpKind::Update => ("upgrade", format!("Updating {id}")),
        OpKind::Remove => ("uninstall", format!("Removing {id}")),
    };
    let mut args = vec![
        verb.to_string(),
        "--exact".to_string(),
        "--id".to_string(),
        id.to_string(),
        "--silent".to_string(),
        "--disable-interactivity".to_string(),
        "--accept-source-agreements".to_string(),
    ];
    if kind != OpKind::Remove {
        // Nothing is being agreed to when something is taken off.
        args.push("--accept-package-agreements".to_string());
    }
    Step {
        source: SourceKind::Winget,
        title,
        command: Command {
            program: "winget.exe".to_string(),
            args,
            env: Vec::new(),
            cwd: None,
        },
        // winget asks for elevation itself when the manifest needs it, and
        // the step is marked so the confirm dialog can say so first. Choosing
        // `--scope user` per package needs the manifest, which is the
        // metadata ladder in a later plan.
        needs_root: true,
        weight: 10,
    }
}
```

And `plan()`:

```rust
fn plan(&self, op: &crate::model::Op) -> Result<Vec<Step>> {
    use crate::model::Op;
    let step = match op {
        Op::Install { package } if package.source == SourceKind::Winget => {
            operation_step(OpKind::Install, &package.id)
        }
        Op::Update { package } if package.source == SourceKind::Winget => {
            operation_step(OpKind::Update, &package.id)
        }
        Op::Remove { package } if package.source == SourceKind::Winget => {
            operation_step(OpKind::Remove, &package.id)
        }
        Op::UpdateAll { source } if *source == SourceKind::Winget => {
            let mut s = operation_step(OpKind::Update, "");
            s.title = "Updating everything winget can".to_string();
            s.command.args = vec![
                "upgrade".to_string(),
                "--all".to_string(),
                "--silent".to_string(),
                "--disable-interactivity".to_string(),
                "--accept-source-agreements".to_string(),
                "--accept-package-agreements".to_string(),
            ];
            s
        }
        // Refresh is Brokey's own catalogue, not winget's, and `catalogue`
        // fetches it when it is stale. There is nothing to run.
        _ => return Ok(Vec::new()),
    };
    Ok(vec![step])
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p brokey-core winget`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/brokey-core/src/sources/windows/winget/
git commit -m "winget: plan an install, an update and a removal"
```

---

### Task 8: Give the machine the source

**Files:**
- Modify: `crates/brokey-core/src/sources/mod.rs`
- Modify: `frontend/src/types.ts` and `crates/brokey/tests/contract.rs` if the
  contract test does not already carry a winget sample
- Modify: `README.md`, `CLAUDE.md`

**Interfaces:**
- Consumes: `windows::winget::Winget::new`.

- [ ] **Step 1: Write the failing test**

In `crates/brokey-core/src/sources/mod.rs`'s test module, replace the existing
`windows_has_the_add_remove_programs_source` test with:

```rust
#[cfg(windows)]
#[test]
fn windows_has_add_remove_programs_and_winget() {
    let system = crate::system::detect();
    let sources = super::all(
        &system,
        crate::http::Client::shared(),
        None,
        &Default::default(),
    );
    let kinds: Vec<_> = sources.iter().map(|s| s.kind()).collect();
    assert!(kinds.contains(&crate::model::SourceKind::Arp), "{kinds:?}");
    assert!(kinds.contains(&crate::model::SourceKind::Winget), "{kinds:?}");
}
```

Check the existing test's exact argument list before editing and keep it; the
four arguments above are what plan 1 left and may have moved.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p brokey-core sources::tests`
Expected: FAIL on Windows, `kinds` has only `Arp`. On Linux the test does not
compile in, so run it on Windows or accept that this one step is verified
there only, and say so.

- [ ] **Step 3: Register the source**

In `sources/mod.rs`'s `#[cfg(windows)]` arm of `all()`:

```rust
#[cfg(windows)]
{
    let _ = (system, catalogue, preferences);
    vec![
        Box::new(windows::arp::Arp::new()),
        Box::new(windows::winget::Winget::new(client)),
    ]
}
```

The `let _ = (...)` line drops whichever arguments are still unused; `client`
is now used, so take it out of that tuple.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets && cargo fmt --all --check`
Expected: all green, on both platforms if both are reachable.

- [ ] **Step 5: Update the two documents that claim Windows has one source**

`CLAUDE.md`'s "What this is" says Windows has one source. It now has two;
say what winget does and that nothing can be run yet because the helper has no
Windows body. `README.md`'s "What is not there yet" is the user-facing list and
is kept honest: winget searches and lists, and cannot yet install, because the
privilege path is the next plan.

- [ ] **Step 6: Commit**

```bash
git add crates/brokey-core/src/sources/mod.rs CLAUDE.md README.md
git commit -m "Brokey searches winget on Windows"
```

---

## Self-review

Run against the spec after the plan was written.

**Spec coverage.** The spec's winget paragraph is Tasks 1, 2, 5 and 7. Its
"Setting a manager up from inside Brokey" row for winget is Task 4, including
the verification invariant. Its grouping section's claim about
`AppsAndFeaturesEntries` is Task 6, which is where the ProductCode and
UpgradeCode join lives. Its "Metadata and icons" ladder is **not** here and is
deliberately out of scope: every field the index does not carry stays `None`,
with a test that says so, and the ladder is plan 3. Its Privilege section is
the next plan, and Task 7 marks winget steps `needs_root: true` so that when
the privilege path lands they route correctly.

**Gaps, recorded rather than hidden.**

1. `--scope user` is not set per package. The spec asks for it "where the
   manifest permits it", and knowing what the manifest permits means fetching
   manifests, which is the metadata ladder. Task 7 says so in a comment and
   marks every operation as elevating, which is the safe direction: the confirm
   dialog over-warns rather than under-warns.
2. `refresh_index()` is left at its default. `catalogue()` refetches when the
   cached copy is older than a day, so the index does refresh, but a user
   pressing refresh does not force it. One line in a later plan.
3. The `sources::all` test in Task 8 only compiles on Windows. Nothing in this
   plan can fix that, and pretending otherwise would be worse.

**Type consistency.** `query::Row` is produced in Task 2 and consumed in 5 and
6. `to_package` is defined in Task 5 and used in Task 6. `version::newer` is
defined in Task 3 and used in Task 6. `bootstrap_steps` and `Bootstrap` are
defined in Task 4 and used only there. `OpKind` and `operation_step` are
defined in Task 7 and used only there. `query::one` is introduced in Task 6 and
used only by Task 6's two statements; it is added there rather than in Task 2
because Task 2 has no caller for it.

**One thing the implementer of Task 6 must check.** `RawEntry`'s field list in
the test helper is copied from `arp.rs` as it stands today. If a field has been
added since, the struct literal will not compile. Read `arp.rs` first and fill
in what is there rather than deleting fields to make it build.

