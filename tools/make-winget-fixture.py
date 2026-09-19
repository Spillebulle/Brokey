#!/usr/bin/env python3
"""Build the small winget catalogue the tests read.

    python3 tools/make-winget-fixture.py

Writes crates/brokey-core/tests/fixtures/windows/winget/source2.msix: a zip of
the same shape as Microsoft's, holding a SQLite database with the real schema
version 2 tables and nine packages chosen to exercise every ranking rule.

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
