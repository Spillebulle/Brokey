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
/// The rank says how it matched; ties break on the length of the id, shortest
/// first, then on the id. Both halves are load-bearing and both have a test:
/// without the length tiebreak `firefox` buries `Mozilla.Firefox` under a
/// hundred locale builds that share its moniker.
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
ORDER BY rank, length(p.id), p.id
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
    let mut stmt = db
        .prepare_cached(SEARCH_SQL)
        .map_err(|e| failed("read", e))?;
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
    let mut stmt = db
        .prepare_cached(BY_ID_SQL)
        .map_err(|e| failed("read", e))?;
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
        std::fs::write(
            &path,
            super::super::index::database_from_msix(&bytes).unwrap(),
        )
        .unwrap();
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

    /// The case the id-length tiebreak exists for. The locale build shares
    /// the base package's moniker and matches at the same rank; without the
    /// tiebreak it wins on name length and the real Firefox is never shown.
    #[test]
    fn the_base_package_beats_its_locale_variant() {
        let (_d, db) = db();
        let rows = search(&db, "firefox", 10).unwrap();
        assert_eq!(
            ids(&rows),
            vec!["Mozilla.Firefox", "Mozilla.Firefox.af"],
            "shortest id first, so the base package leads"
        );
    }

    /// The tiebreak must not reorder things that matched differently: a rank
    /// 0 hit stays ahead of a rank 4 hit whatever the ids are.
    #[test]
    fn a_better_match_beats_a_shorter_id() {
        let (_d, db) = db();
        let rows = search(&db, "steam", 10).unwrap();
        assert_eq!(ids(&rows)[0], "Valve.Steam");
    }

    /// Case folds. Nobody types a package id with its capitals.
    #[test]
    fn matching_ignores_case() {
        let (_d, db) = db();
        assert_eq!(
            ids(&search(&db, "OBSIDIAN", 10).unwrap())[0],
            "Obsidian.Obsidian"
        );
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
        assert_eq!(count(&db).unwrap(), 8);
    }

    /// Normalisation matches the index's own: letters and digits, folded.
    #[test]
    fn normalising_keeps_letters_and_digits_only() {
        assert_eq!(normalise("Mozilla Firefox (en-US)"), "mozillafirefoxenus");
        assert_eq!(normalise("Notepad++"), "notepad");
        assert_eq!(normalise("  7-Zip  "), "7zip");
    }
}
