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
        Error::new(format!(
            "The winget catalogue is not a readable package: {e}."
        ))
    })?;
    let mut member = zip.by_name(DATABASE_MEMBER).map_err(|_| {
        Error::new(format!(
            "The winget catalogue has no {DATABASE_MEMBER} in it, so there is no index to read."
        ))
    })?;
    let mut out = Vec::with_capacity(member.size() as usize);
    member
        .read_to_end(&mut out)
        .map_err(|e| Error::new(format!("The winget catalogue could not be unpacked: {e}.")))?;
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
        assert!(
            e.message.contains('1'),
            "names what it found: {}",
            e.message
        );
        assert!(e.message.contains('2'), "and what it wants: {}", e.message);
    }
}
