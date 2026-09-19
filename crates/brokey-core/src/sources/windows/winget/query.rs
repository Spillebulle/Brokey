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
    /// From `norm_publishers2`, already normalised by the index. This is what
    /// the registry join in Task 6 matches against, and it travels with the
    /// row for that reason.
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

/// Run a statement and collect its rows. The caller decides what more than
/// one of them means; the statement's own `LIMIT` decides how many can come
/// back at all.
pub fn rows(db: &rusqlite::Connection, sql: &str, params: &[(&str, &String)]) -> Result<Vec<Row>> {
    let mut stmt = db.prepare_cached(sql).map_err(|e| failed("read", e))?;
    let bound: Vec<(&str, &dyn rusqlite::ToSql)> = params
        .iter()
        .map(|(k, v)| (*k, *v as &dyn rusqlite::ToSql))
        .collect();
    let found = stmt
        .query_map(bound.as_slice(), row_from)
        .map_err(|e| failed("read", e))?;
    found
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| failed("read", e))
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
        assert_eq!(
            ids(&search(&db, "OBSIDIAN", 10).unwrap())[0],
            "Obsidian.Obsidian"
        );
    }

    /// The version family is returned whole rather than ranked. The publisher
    /// travels with the row because Task 6's registry join matches on it, so
    /// it has to come back with every row.
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

    /// The identifying fields the index really has.
    #[test]
    fn the_package_carries_what_the_index_knows() {
        let (_d, db) = db();
        let row = by_id(&db, "Python.Python.3.14").unwrap().unwrap();
        let p = super::super::to_package(&row);
        assert_eq!(p.source, crate::model::SourceKind::Winget);
        assert_eq!(p.id, "Python.Python.3.14");
        assert_eq!(p.name, "Python 3.14");
        assert_eq!(p.version.as_deref(), Some("3.14.2"));
    }

    /// The publisher in the index is `pythonsoftwarefoundation`, which is a join
    /// key and not a name. Showing it would put a mangled word on the detail
    /// page under "Developer", so the source says it does not know instead.
    #[test]
    fn the_normalised_publisher_is_not_shown_as_the_developer() {
        let (_d, db) = db();
        let row = by_id(&db, "7zip.7zip").unwrap().unwrap();
        assert_eq!(row.publisher.as_deref(), Some("igorpavlov"));
        assert!(super::super::to_package(&row).developer.is_none());
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

    fn entry(
        key: &str,
        name: &str,
        version: &str,
        publisher: &str,
    ) -> crate::sources::windows::arp::RawEntry {
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
        let e = entry(
            "{23170F69-40C1-2701-2603-000001000000}",
            "7-Zip 26.03 (x64)",
            "26.03",
            "Igor Pavlov",
        );
        let row = super::super::match_entry(&db, &e).unwrap().unwrap();
        assert_eq!(row.id, "7zip.7zip");
    }

    /// Not every product code is a GUID. The column holds whatever the key is
    /// named, and `notepad++` is a real row in the real catalogue.
    #[test]
    fn a_product_code_that_is_not_a_guid_still_matches() {
        let (_d, db) = db();
        let e = entry(
            "notepad++",
            "Notepad++ (64-bit x64)",
            "8.9.8",
            "Notepad++ Team",
        );
        assert_eq!(
            super::super::match_entry(&db, &e).unwrap().unwrap().id,
            "Notepad++.Notepad++"
        );
    }

    /// The upgrade code rung, for an entry whose product code changed between
    /// versions but whose upgrade code did not.
    #[test]
    fn an_upgrade_code_matches_when_the_product_code_does_not() {
        let (_d, db) = db();
        let e = entry(
            "{A1B2C3D4-0000-0000-0000-00000000F00D}",
            "Something Else",
            "1.0",
            "Nobody",
        );
        assert_eq!(
            super::super::match_entry(&db, &e).unwrap().unwrap().id,
            "Notepad++.Notepad++"
        );
    }

    /// The name rung fires only with the publisher beside it.
    #[test]
    fn a_name_matches_when_the_publisher_agrees() {
        let (_d, db) = db();
        let e = entry("Obsidian_is_not_a_code", "Obsidian", "1.13.0", "Obsidian");
        assert_eq!(
            super::super::match_entry(&db, &e).unwrap().unwrap().id,
            "Obsidian.Obsidian"
        );
    }

    /// And not without it. This is the rung that would otherwise join every
    /// Python to every other Python, and a wrong join offers an update that
    /// replaces one application with a different one.
    #[test]
    fn a_name_alone_is_not_enough() {
        let (_d, db) = db();
        let e = entry(
            "SomeKey",
            "Obsidian",
            "1.13.0",
            "A Different Company Entirely",
        );
        assert!(super::super::match_entry(&db, &e).unwrap().is_none());
    }

    /// The index folds version families, so a name that reaches the whole family
    /// reaches all of it at once. Choosing one would offer an update that
    /// replaces the installed Python with a different Python, so it chooses none.
    #[test]
    fn a_name_that_matches_a_whole_version_family_is_not_a_match() {
        let (_d, db) = db();
        let e = entry(
            "NotAProductCode",
            "Python",
            "3.14.2",
            "Python Software Foundation",
        );
        assert!(super::super::match_entry(&db, &e).unwrap().is_none());
    }

    /// What the name rung is actually worth, written down so nobody mistakes it
    /// for more. Brokey's `normalise` folds to letters and digits; winget's own
    /// normaliser, which produced the `norm_names2` values, also strips versions,
    /// architectures and locale tags, so it stored `python` for `Python 3.0`.
    /// A real registry DisplayName carries all of those, so the two disagree and
    /// the rung misses. It misses safely, and the exact rung below is what
    /// actually finds software installed through winget.
    #[test]
    fn a_realistic_display_name_misses_the_name_rung_and_the_product_code_saves_it() {
        let (_d, db) = db();
        let named = entry(
            "NotAProductCode",
            "Python 3.14.2 (64-bit)",
            "3.14.2",
            "Python Software Foundation",
        );
        assert!(
            super::super::match_entry(&db, &named).unwrap().is_none(),
            "the name rung is not expected to reach a versioned display name"
        );

        let keyed = entry(
            "{6B1C1B1E-0000-0000-0000-000000000314}",
            "Python 3.14.2 (64-bit)",
            "3.14.2",
            "Python Software Foundation",
        );
        assert_eq!(
            super::super::match_entry(&db, &keyed).unwrap().unwrap().id,
            "Python.Python.3.14"
        );
    }

    /// Something the catalogue has never heard of.
    #[test]
    fn an_unmatched_entry_is_none() {
        let (_d, db) = db();
        let e = entry(
            "NothingLikeThis",
            "Bespoke Internal Tool",
            "4.2",
            "Our IT Department",
        );
        assert!(super::super::match_entry(&db, &e).unwrap().is_none());
    }

    /// An older installed version against the catalogue's newest is an update,
    /// and it names both ends.
    #[test]
    fn an_older_installed_version_is_an_update() {
        let (_d, db) = db();
        let e = entry(
            "bd400747-f0c1-5638-a859-982036102edf",
            "Obsidian",
            "1.10.0",
            "Obsidian",
        );
        let ups = super::super::updates_from(&db, std::slice::from_ref(&e)).unwrap();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].from.as_deref(), Some("1.10.0"));
        assert_eq!(ups[0].to, "1.13.7");
    }

    /// The same version is not an update, and neither is a newer installed one.
    #[test]
    fn an_up_to_date_entry_is_not_an_update() {
        let (_d, db) = db();
        let same = entry(
            "bd400747-f0c1-5638-a859-982036102edf",
            "Obsidian",
            "1.13.7",
            "Obsidian",
        );
        assert!(
            super::super::updates_from(&db, std::slice::from_ref(&same))
                .unwrap()
                .is_empty()
        );
        let ahead = entry(
            "bd400747-f0c1-5638-a859-982036102edf",
            "Obsidian",
            "2.0.0",
            "Obsidian",
        );
        assert!(
            super::super::updates_from(&db, std::slice::from_ref(&ahead))
                .unwrap()
                .is_empty()
        );
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
}
