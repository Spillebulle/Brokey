//! Packages from several sources that are the same application become one
//! [`App`] with several editions. Pure: a list in, a list out, nothing read
//! from the machine, so every rule is fixture-tested in `tests/group.rs`.
//!
//! The join is decided in two passes. The AppStream component id is the
//! certain one: two packages that carry the same id are the same application
//! whatever their sources say, and a Flatpak's ref carries its id by
//! definition. The name is the uncertain one. Every package answers to a few
//! normalised keys: its display name, its package name and the last segment
//! of its AppStream id, so pacman's `gimp` (named "GNU Image Manipulation
//! Program" by the catalogue), Flathub's `org.gimp.GIMP`, the Snap Store's
//! `gimp` and the AUR's `gimp-git` all answer to `gimp`. Two packages from
//! different sources that share a key join when one of them is an
//! application or their summaries agree, and such an edition carries a
//! confidence below 1 so the page can say "matched by name". Two packages
//! from one source never join each other by name: the AUR's `yay` and
//! `yay-bin` may well be the same program, but the source that lists both is
//! the authority on that. They share a row only when an edition from a third
//! source matches both, as pacman's `firefox` does for the AUR's
//! `firefox-git` and `firefox-nightly`, and each such member carries its own
//! name confidence. Nor do two packages whose catalogues gave them different
//! reverse-DNS ids: GNOME's "Files" and elementary's "Files" share a name and
//! nothing else. And no bridge joins two packages one source gave different
//! ids to: the Arch catalogue's `element` (an audio plugin host) and
//! `element-desktop` (the Matrix client) stay apart however many AUR builds
//! answer to `element`.
//!
//! Adding a heuristic means adding a fixture where it fires and one where it
//! must not (`CLAUDE.md`).

use crate::model::*;
use std::collections::HashMap;

/// Group packages into apps, rank them against the query, and sort.
///
/// The output order is the page's default: relevance first, applications
/// before plain packages, then name. An empty query sorts by name alone and
/// leaves every relevance at 0.
/// [`group`], with the editions the user has split out kept apart. `split`
/// holds `source:id` words (the `split` setting); each such package becomes
/// a row of its own instead of joining whatever it would have matched, and
/// the rows are sorted together with the rest.
pub fn group_with(packages: Vec<Package>, query: &str, split: &[String]) -> Vec<App> {
    if split.is_empty() {
        return group(packages, query);
    }
    let (apart, together): (Vec<Package>, Vec<Package>) = packages.into_iter().partition(|p| {
        split
            .iter()
            .any(|s| s == &format!("{}:{}", p.source.id(), p.id))
    });
    let mut apps = group(together, query);
    for package in apart {
        apps.extend(group(vec![package], query));
    }
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        apps.sort_by_key(|a| a.name.to_lowercase());
    } else {
        apps.sort_by(|a, b| {
            b.relevance
                .total_cmp(&a.relevance)
                .then_with(|| kind_rank(a.kind).cmp(&kind_rank(b.kind)))
                .then_with(|| named_by_source(b, &query).cmp(&named_by_source(a, &query)))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }
    make_keys_unique(&mut apps);
    apps
}

pub fn group(packages: Vec<Package>, query: &str) -> Vec<App> {
    if packages.is_empty() {
        return Vec::new();
    }
    let facts: Vec<Facts> = packages.iter().map(Facts::of).collect();
    let mut sets = Sets::new(&facts);

    // Pass one: the AppStream id. Unconditional, across and within sources.
    // The Arch catalogue lists `org.gnu.emacs` under both `emacs` and
    // `emacs-nox`, and those are one application in two builds; a Flatpak
    // remote's stable and beta branches of one id are likewise editions.
    let mut by_appstream: HashMap<&str, usize> = HashMap::new();
    for (i, f) in facts.iter().enumerate() {
        if let Some(id) = &f.appstream_key {
            match by_appstream.get(id.as_str()) {
                Some(&first) => {
                    sets.union(first, i);
                }
                None => {
                    by_appstream.insert(id, i);
                }
            }
        }
    }

    // Pass two: the name keys. Only pairs sharing a key are compared, so the
    // cost is the size of each bucket, not the square of the input. Buckets
    // are small in practice: a key is shared by at most one package per
    // source plus that source's own variants, which never pair up. The pairs
    // are sorted before they are tried because a join can be refused (see
    // `Sets::union`), which makes the outcome depend on the order, and a
    // HashMap's order is different on every run.
    let mut by_key: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, f) in facts.iter().enumerate() {
        for key in &f.keys {
            by_key.entry(key.as_str()).or_default().push(i);
        }
    }
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for bucket in by_key.values().filter(|b| b.len() > 1) {
        for (n, &i) in bucket.iter().enumerate() {
            for &j in &bucket[n + 1..] {
                pairs.push((i.min(j), i.max(j)));
            }
        }
    }
    pairs.sort_unstable();
    pairs.dedup();
    let mut name_confidence = vec![0.0f32; packages.len()];
    for (i, j) in pairs {
        let Some(confidence) = name_match(&packages[i], &facts[i], &packages[j], &facts[j]) else {
            continue;
        };
        if sets.union(i, j) {
            name_confidence[i] = name_confidence[i].max(confidence);
            name_confidence[j] = name_confidence[j].max(confidence);
        }
    }

    // Collect the sets. A set's root is its lowest index, so walking the
    // roots in order keeps groups in the order their first member arrived.
    let mut members: Vec<Vec<usize>> = vec![Vec::new(); packages.len()];
    for i in 0..packages.len() {
        let root = sets.find(i);
        members[root].push(i);
    }
    let mut slots: Vec<Option<Package>> = packages.into_iter().map(Some).collect();
    let mut apps: Vec<(App, f32)> = Vec::new();
    let normalised_query = normalise_key(query);
    let query = query.trim().to_lowercase();
    for group in members.into_iter().filter(|g| !g.is_empty()) {
        let editions: Vec<(Edition, &Facts)> = group
            .iter()
            .map(|&i| {
                let package = slots[i]
                    .take()
                    .expect("each package belongs to exactly one group");
                let (matched_by, confidence) = if group.len() == 1 {
                    (MatchedBy::Alone, 1.0)
                } else if shares_appstream_id(i, &group, &facts) {
                    (MatchedBy::AppStream, 1.0)
                } else {
                    // A member of a group of two or more that has no
                    // AppStream twin got there through a name pair, so a
                    // confidence was recorded; the fallback only guards the
                    // invariant, it is not a path.
                    let recorded = name_confidence[i];
                    (MatchedBy::Name, if recorded > 0.0 { recorded } else { 0.6 })
                };
                let edition = Edition {
                    package,
                    matched_by,
                    confidence,
                };
                (edition, &facts[i])
            })
            .collect();
        let (app, keys) = assemble(editions);
        let score = if query.is_empty() {
            0.0
        } else {
            relevance(&app, &keys, &query, &normalised_query)
        };
        apps.push((app, score));
    }

    if query.is_empty() {
        apps.sort_by(|(a, _), (b, _)| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.key.cmp(&b.key))
        });
    } else {
        apps.sort_by(|(a, sa), (b, sb)| {
            sb.total_cmp(sa)
                .then_with(|| kind_rank(a.kind).cmp(&kind_rank(b.kind)))
                .then_with(|| named_by_source(b, &query).cmp(&named_by_source(a, &query)))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                .then_with(|| a.key.cmp(&b.key))
        });
    }
    let mut apps: Vec<App> = apps
        .into_iter()
        .map(|(mut app, score)| {
            // The bonuses can push an exact match past 1; the sort used the
            // full score, the page gets the documented range.
            app.relevance = score.min(1.0);
            app
        })
        .collect();
    make_keys_unique(&mut apps);
    apps
}

/// The normalised name key: lower-case, a reverse-DNS id reduced to its last
/// segment, one edition suffix removed, everything but ASCII letters and
/// digits dropped. `steam`, `Steam`, `steam-git` and
/// `com.valvesoftware.Steam` are all `steam`; `steam-native-runtime` is
/// `steamnativeruntime`, a different key, which is the point.
pub fn normalise_key(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let base = strip_desktop(&lower);
    let base = strip_reverse_dns(base);
    let base = strip_edition_suffix(base);
    alphanumeric(base)
}

/// The AppStream id a package is matched on, in the case the source gave
/// it, with a trailing `.desktop` removed. A Flatpak without one is keyed on
/// its ref's id segment, because that segment is the component id by
/// Flatpak's own rules.
pub fn appstream_id(package: &Package) -> Option<String> {
    let raw = match &package.appstream_id {
        Some(id) if !id.trim().is_empty() => id.trim(),
        _ if package.source == SourceKind::Flatpak => flatpak_ref_id(&package.id)?,
        _ => return None,
    };
    let id = strip_desktop(raw);
    (!id.is_empty()).then(|| id.to_string())
}

/// Names that share a key with a program they are not. Each is the raw name
/// of a package in the Arch repositories or the AUR whose suffix reads as
/// an edition but marks a shim or an unrelated program. The first of each
/// pair is what it would wrongly join; the second never joins anything but
/// its own editions (`wl-clipboard-x11-git` is still `wl-clipboard-x11`).
///
/// Not on the list, and checked: `python-git`, `perl-git`, `nodejs-git`,
/// `rust-git`, `go-bin` and `yay-bin` are the same programs as their bases.
/// The toolkit names (`fcitx5-qt`, `bluez-qt`, `colord-gtk`, `discord-qt`,
/// `neovim-gtk` and a dozen more) were on this list while `-qt` and `-gtk`
/// were edition suffixes; they are gone because those suffixes are, see
/// [`EDITION_SUFFIXES`].
const FALSE_FRIENDS: &[(&str, &str)] = &[
    // An X11 shim for a Wayland library or tool, not the thing itself.
    ("libxkbcommon", "libxkbcommon-x11"),
    ("wl-clipboard", "wl-clipboard-x11"),
    // A library about git, not the language from git.
    ("ruby", "ruby-git"),
];

/// Edition suffixes, stripped once from the end of a name when at least
/// three letters or digits remain. `-canary`, `-beta`, `-devel` and
/// `-staging` are deliberately absent: a canary, beta or staging build is a
/// different release channel (`discord-canary` and `signal-desktop-beta`
/// are rows of their own), and the AUR's `-devel` packages are as often a
/// separate upstream branch as a newer build. `-qt` and `-gtk` are absent
/// too: on real data they name a different program far more often than an
/// edition (`neovim-qt` and `neovim-gtk` are front-ends, not Neovim;
/// `bluez-qt` is a binding, not BlueZ), and the editions they did name
/// (`wireshark-qt`, `transmission-gtk`) meet their siblings through the
/// catalogue's AppStream id, which needs no suffix rule.
const EDITION_SUFFIXES: &[&str] = &[
    "-bin",
    "-git",
    "-appimage",
    "-nightly",
    "-stable",
    "-electron",
    "-wayland",
    "-x11",
];

/// Last segments of a reverse-DNS id that name nothing on their own. For
/// these the vendor segment stays in the key: `org.telegram.desktop` is
/// `telegramdesktop`, which is what pacman calls the package, rather than
/// `desktop`, which is what a dozen unrelated things would share.
const GENERIC_SEGMENTS: &[&str] = &["desktop", "client", "app", "gui", "launcher"];

/// Words that say nothing about which program a summary describes. Two
/// summaries must share two words outside this list before a name match
/// between two plain packages is believed.
const STOP_WORDS: &[&str] = &[
    "about",
    "also",
    "application",
    "applications",
    "based",
    "been",
    "being",
    "between",
    "binary",
    "both",
    "build",
    "collection",
    "command",
    "desktop",
    "development",
    "does",
    "each",
    "every",
    "fast",
    "file",
    "files",
    "framework",
    "free",
    "from",
    "graphical",
    "have",
    "implementation",
    "interface",
    "into",
    "just",
    "latest",
    "library",
    "libraries",
    "lightweight",
    "like",
    "linux",
    "made",
    "make",
    "makes",
    "many",
    "modern",
    "module",
    "modules",
    "more",
    "most",
    "much",
    "official",
    "only",
    "open",
    "other",
    "over",
    "package",
    "packages",
    "plugin",
    "plugins",
    "program",
    "programs",
    "release",
    "simple",
    "small",
    "software",
    "some",
    "source",
    "such",
    "support",
    "supported",
    "supports",
    "system",
    "than",
    "that",
    "their",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "tool",
    "toolkit",
    "tools",
    "unofficial",
    "used",
    "user",
    "users",
    "uses",
    "using",
    "utilities",
    "utility",
    "version",
    "very",
    "what",
    "when",
    "where",
    "which",
    "while",
    "will",
    "with",
    "without",
    "written",
    "your",
];

/// What the grouper needs to know about a package, computed once.
struct Facts {
    /// Where the package comes from, for the per-source id check in
    /// [`Sets::union`].
    source: SourceKind,
    /// Lower-cased AppStream id for matching, `None` when the package has
    /// none and is not a Flatpak app.
    appstream_key: Option<String>,
    /// Every normalised key the package answers to: its display name, its
    /// package name, its AppStream id's last segment and, for a false
    /// friend, its unstripped name. Never empty.
    keys: Vec<String>,
    /// Which false friend this package is, if it is one.
    impostor: Option<&'static str>,
    /// Significant summary words, sorted and deduplicated.
    words: Vec<String>,
}

impl Facts {
    fn of(package: &Package) -> Facts {
        let name_lower = package.name.trim().to_lowercase();
        let id_lower = package.id.trim().to_lowercase();
        let impostor = impostor_of(&name_lower).or_else(|| impostor_of(&id_lower));
        let mut keys = Vec::with_capacity(4);
        push_key(&mut keys, normalise_key(&package.name));
        push_key(&mut keys, normalise_key(source_name(package)));
        let appstream = appstream_id(package);
        if let Some(id) = &appstream {
            push_key(&mut keys, normalise_key(id));
        }
        // A false friend's key loses its suffix like any other, which is
        // how it meets the program it is not; its own editions meet it on
        // the unstripped name.
        if let Some(impostor) = impostor {
            push_key(&mut keys, alphanumeric(impostor));
        }
        if keys.is_empty() {
            // A name with no letters or digits joins nothing; the colon
            // keeps this key apart from every real one.
            keys.push(format!("{}:{}", package.source.id(), package.id));
        }
        Facts {
            source: package.source,
            appstream_key: appstream.map(|id| id.to_lowercase()),
            keys,
            impostor,
            words: package
                .summary
                .as_deref()
                .map(significant_words)
                .unwrap_or_default(),
        }
    }
}

fn alphanumeric(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).collect()
}

/// The key an app without an AppStream id is named by: its name's key, or
/// for a false friend the unstripped name, so `wl-clipboard-x11` is
/// `name:wlclipboardx11` and not a second `name:wlclipboard`.
fn app_name_key(name: &str) -> String {
    match impostor_of(&name.trim().to_lowercase()) {
        Some(impostor) => alphanumeric(impostor),
        None => normalise_key(name),
    }
}

fn push_key(keys: &mut Vec<String>, key: String) {
    if !key.is_empty() && !keys.contains(&key) {
        keys.push(key);
    }
}

/// The source's own name for the package, which is the id for every source
/// but two: a Flatpak ref carries remote, kind and architecture around its
/// id, and a GitHub id is `owner/repo`.
fn source_name(package: &Package) -> &str {
    match package.source {
        SourceKind::Flatpak => flatpak_ref_name(&package.id).unwrap_or(&package.id),
        SourceKind::Github => package.id.rsplit('/').next().unwrap_or(&package.id),
        _ => &package.id,
    }
}

/// Whether one of the app's editions is called exactly the query by its own
/// source. The sort's tie-break between two apps that answer the query
/// equally well: pacman's `docker` outranks `cockpit-docker`, which the
/// catalogue names "Docker" too. `query` is trimmed and lower-cased.
fn named_by_source(app: &App, query: &str) -> bool {
    app.editions
        .iter()
        .any(|e| source_name(&e.package).to_lowercase() == query)
}

/// Whether two packages sharing a name key are the same application, and
/// how sure that is. `None` means keep them apart.
fn name_match(a: &Package, fa: &Facts, b: &Package, fb: &Facts) -> Option<f32> {
    if a.source == b.source {
        return None;
    }
    // A false friend joins only its own editions: `fcitx5-qt` and
    // `fcitx5-qt-git` are one thing, `fcitx5` is another.
    if fa.impostor != fb.impostor {
        return None;
    }
    // An application vouches for a plain package of the same name, never
    // for a runtime, font, add-on, driver or firmware: those are not
    // editions of anything, whatever they are called.
    let is_app = |p: &Package| p.kind == PackageKind::App;
    let is_plain = |p: &Package| matches!(p.kind, PackageKind::App | PackageKind::Package);
    if (is_app(a) && is_plain(b)) || (is_app(b) && is_plain(a)) {
        return Some(0.8);
    }
    (shared_words(&fa.words, &fb.words) >= 2).then_some(0.6)
}

/// The false friend a name is, with one edition suffix allowed on it.
fn impostor_of(name_lower: &str) -> Option<&'static str> {
    let candidates = [name_lower, strip_edition_suffix(name_lower)];
    FALSE_FRIENDS
        .iter()
        .map(|(_, impostor)| *impostor)
        .find(|impostor| candidates.contains(impostor))
}

/// Whether member `i` of `group` has an AppStream id another member shares.
fn shares_appstream_id(i: usize, group: &[usize], facts: &[Facts]) -> bool {
    match &facts[i].appstream_key {
        Some(id) => group
            .iter()
            .any(|&j| j != i && facts[j].appstream_key.as_deref() == Some(id.as_str())),
        None => false,
    }
}

/// Count of words in both sorted lists.
fn shared_words(a: &[String], b: &[String]) -> usize {
    let (mut i, mut j, mut shared) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
        }
    }
    shared
}

/// The words of a summary that could tell two programs apart: four letters
/// or more, not in the stop list, lower-cased, sorted, unique.
fn significant_words(summary: &str) -> Vec<String> {
    let mut words: Vec<String> = summary
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_lowercase)
        .filter(|w| !STOP_WORDS.contains(&w.as_str()))
        .collect();
    words.sort();
    words.dedup();
    words
}

/// `com.valvesoftware.Steam.desktop` to `com.valvesoftware.Steam`. The
/// AppStream source already does this; doing it again costs nothing and
/// protects the join from a source that forgets. `org.telegram.desktop` is
/// left whole: a desktop-file suffix on a reverse-DNS id makes four
/// segments or more and on a plain name two, so a three-segment id that
/// ends in `.desktop` is one whose application segment is `desktop`.
fn strip_desktop(id: &str) -> &str {
    let suffix = ".desktop";
    let bytes = id.as_bytes();
    let ends_with_it = bytes.len() > suffix.len()
        && bytes[bytes.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes());
    if !ends_with_it {
        return id;
    }
    let is_three_segment_id = id.matches('.').count() == 2 && is_reverse_dns(id);
    if is_three_segment_id {
        id
    } else {
        &id[..bytes.len() - suffix.len()]
    }
}

/// The segments after the top-level one when `name` is a reverse-DNS id:
/// no spaces, at least three segments, none empty, and a short alphabetic
/// first segment. `python-3.12.1` fails the first-segment test and
/// `gimp.org` is two segments, a host name.
fn reverse_dns_segments(name: &str) -> Option<Vec<&str>> {
    if name.contains(char::is_whitespace) {
        return None;
    }
    let mut segments = name.split('.');
    let first = segments.next()?;
    let tld_like = (2..=8).contains(&first.len()) && first.chars().all(|c| c.is_ascii_alphabetic());
    let rest: Vec<&str> = segments.collect();
    if !tld_like || rest.len() < 2 || rest.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some(rest)
}

fn is_reverse_dns(id: &str) -> bool {
    reverse_dns_segments(id).is_some()
}

/// `com.valvesoftware.steam` to `steam`; `org.telegram.desktop` to
/// `telegram.desktop`, because `desktop` alone names nothing. Anything that
/// is not a reverse-DNS id is left whole.
fn strip_reverse_dns(name: &str) -> &str {
    let Some(rest) = reverse_dns_segments(name) else {
        return name;
    };
    let last = rest[rest.len() - 1];
    if GENERIC_SEGMENTS.contains(&last) {
        let vendor = rest[rest.len() - 2];
        return &name[name.len() - vendor.len() - 1 - last.len()..];
    }
    last
}

/// One edition suffix off the end, when what remains still names something.
fn strip_edition_suffix(name: &str) -> &str {
    for suffix in EDITION_SUFFIXES {
        if let Some(rest) = name.strip_suffix(suffix) {
            let letters = rest.chars().filter(char::is_ascii_alphanumeric).count();
            return if letters >= 3 { rest } else { name };
        }
    }
    name
}

/// The id segment of a Flatpak app ref: `flathub/app/org.gimp.GIMP/x86_64/stable`
/// gives `org.gimp.GIMP`, as does a bare id. Runtimes give `None`: their ids
/// are not application ids.
fn flatpak_ref_id(id: &str) -> Option<&str> {
    let segments: Vec<&str> = id.split('/').filter(|s| !s.is_empty()).collect();
    if let Some(pos) = segments.iter().position(|s| *s == "app") {
        return segments.get(pos + 1).copied().filter(|s| !s.is_empty());
    }
    match segments.as_slice() {
        [only] if only.matches('.').count() >= 2 => Some(only),
        _ => None,
    }
}

/// The name segment of any Flatpak ref, runtimes included, for the name key.
fn flatpak_ref_name(id: &str) -> Option<&str> {
    let segments: Vec<&str> = id.split('/').filter(|s| !s.is_empty()).collect();
    if let Some(pos) = segments.iter().position(|s| *s == "app" || *s == "runtime") {
        return segments.get(pos + 1).copied();
    }
    match segments.as_slice() {
        [only] => Some(only),
        _ => None,
    }
}

/// The order editions are listed in. The distribution's own repository
/// first, then the sandboxed stores, then the AUR and GitHub, which build or
/// download rather than install.
fn edition_rank(source: SourceKind) -> u8 {
    match source {
        SourceKind::Pacman => 0,
        SourceKind::Apt => 1,
        SourceKind::Dnf => 2,
        SourceKind::Flatpak => 3,
        SourceKind::Snap => 4,
        SourceKind::Aur => 5,
        SourceKind::Github => 6,
        SourceKind::Fwupd => 7,
        SourceKind::Chwd => 8,
        SourceKind::Winget => 9,
        SourceKind::Arp => 10,
        SourceKind::Choco => 11,
        SourceKind::Scoop => 12,
        SourceKind::Msix => 13,
        SourceKind::Features => 14,
    }
}

/// Whose icon, developer and categories to prefer. Differs from the edition
/// order because apt and dnf are untested and their metadata is trusted
/// less than the AUR's until they have run on a real machine.
fn metadata_rank(source: SourceKind) -> u8 {
    match source {
        SourceKind::Pacman => 0,
        SourceKind::Flatpak => 1,
        SourceKind::Snap => 2,
        SourceKind::Aur => 3,
        SourceKind::Apt => 4,
        SourceKind::Dnf => 5,
        SourceKind::Github => 6,
        SourceKind::Fwupd => 7,
        SourceKind::Chwd => 8,
        SourceKind::Winget => 9,
        SourceKind::Arp => 10,
        SourceKind::Choco => 11,
        SourceKind::Scoop => 12,
        SourceKind::Msix => 13,
        SourceKind::Features => 14,
    }
}

fn kind_rank(kind: PackageKind) -> u8 {
    if kind == PackageKind::App { 0 } else { 1 }
}

/// Whether a catalogue describes the package: it carries an AppStream id
/// from a source whose metadata is a catalogue's. The AUR and GitHub only
/// borrow an id (by package name, or by the repository's own metainfo);
/// their summaries are a packager's pkgdesc and a repository description.
fn from_catalogue(package: &Package) -> bool {
    appstream_id(package).is_some()
        && !matches!(package.source, SourceKind::Aur | SourceKind::Github)
}

fn display_name(package: &Package) -> &str {
    if package.name.trim().is_empty() {
        &package.id
    } else {
        &package.name
    }
}

/// What the relevance score matches the query against, beyond the app's
/// own fields: every key its editions answer to, and their AppStream ids.
struct Keys {
    keys: Vec<String>,
    ids: Vec<String>,
}

/// One group's editions into an app, with the keys the relevance score
/// needs.
fn assemble(mut editions: Vec<(Edition, &Facts)>) -> (App, Keys) {
    editions.sort_by_key(|(e, _)| edition_rank(e.package.source));
    let mut keys = Keys {
        keys: Vec::new(),
        ids: Vec::new(),
    };
    for (_, f) in &editions {
        for key in &f.keys {
            push_key(&mut keys.keys, key.clone());
        }
        if let Some(id) = &f.appstream_key {
            push_key(&mut keys.ids, id.clone());
        }
    }
    let name = choose_name(&editions);
    let key = match editions.iter().find_map(|(e, _)| appstream_id(&e.package)) {
        Some(id) => id,
        None => {
            let normalised = app_name_key(&name);
            let normalised = if normalised.is_empty() {
                &editions[0].1.keys[0]
            } else {
                &normalised
            };
            format!("name:{normalised}")
        }
    };
    let kind = if editions
        .iter()
        .any(|(e, _)| e.package.kind == PackageKind::App)
    {
        PackageKind::App
    } else {
        editions[0].0.package.kind
    };

    let mut by_metadata: Vec<&Package> = editions.iter().map(|(e, _)| &e.package).collect();
    by_metadata.sort_by_key(|p| metadata_rank(p.source));
    // The summary describes the application, so when a catalogue describes
    // any edition only those compete: an AUR pkgdesc is longer than a
    // catalogue summary and describes the build ("Static binaries from
    // upstream"), not the program. Among the candidates the longest wins;
    // on a tie the preferred source keeps it, which `max_by_key` would not
    // do (it returns the last maximum).
    let longest = |candidates: &[&Package]| -> Option<String> {
        let mut best: Option<&str> = None;
        for s in candidates
            .iter()
            .filter_map(|p| p.summary.as_deref())
            .filter(|s| !s.trim().is_empty())
        {
            if best.is_none_or(|b| s.chars().count() > b.chars().count()) {
                best = Some(s);
            }
        }
        best.map(str::to_string)
    };
    let catalogue: Vec<&Package> = by_metadata
        .iter()
        .copied()
        .filter(|p| from_catalogue(p))
        .collect();
    let summary = longest(&catalogue).or_else(|| longest(&by_metadata));
    let icon = by_metadata.iter().find_map(|p| p.icon.clone());
    let developer = by_metadata
        .iter()
        .find_map(|p| p.developer.as_deref().filter(|d| !d.trim().is_empty()))
        .map(str::to_string);
    let categories = by_metadata
        .iter()
        .find(|p| !p.categories.is_empty())
        .map(|p| p.categories.clone())
        .unwrap_or_default();

    let packages = editions.iter().map(|(e, _)| &e.package);
    let installed = packages.clone().any(|p| p.installed);
    let updated = packages.clone().filter_map(|p| p.updated).max();
    let popularity = packages.filter_map(|p| p.popularity).reduce(f64::max);

    let app = App {
        key,
        name,
        kind,
        summary,
        icon,
        developer,
        categories,
        installed,
        updated,
        popularity,
        relevance: 0.0,
        editions: editions.into_iter().map(|(e, _)| e).collect(),
    };
    (app, keys)
}

/// The application edition's name, else the shortest. Among application
/// editions one whose name is not just its id wins, so a pacman `steam`
/// beside a Flathub `Steam` is drawn as "Steam"; plain edition order would
/// pick whichever source came first, which for a source that names by
/// package name is the id.
fn choose_name(editions: &[(Edition, &Facts)]) -> String {
    let apps = || {
        editions
            .iter()
            .map(|(e, _)| &e.package)
            .filter(|p| p.kind == PackageKind::App)
    };
    if let Some(p) = apps().find(|p| !p.name.trim().is_empty() && p.name != p.id) {
        return p.name.clone();
    }
    if let Some(p) = apps().next() {
        return display_name(p).to_string();
    }
    editions
        .iter()
        .map(|(e, _)| display_name(&e.package))
        .min_by_key(|n| n.chars().count())
        .unwrap_or_default()
        .to_string()
}

/// How well an app answers the query, 0.2 to a little over 1. `query` is
/// trimmed and lower-cased; `normalised` is its name key.
fn relevance(app: &App, keys: &Keys, query: &str, normalised: &str) -> f32 {
    let names: Vec<String> = std::iter::once(app.name.as_str())
        .chain(app.editions.iter().map(|e| e.package.name.as_str()))
        .map(str::to_lowercase)
        .collect();
    let summaries: Vec<String> = std::iter::once(app.summary.as_deref())
        .chain(app.editions.iter().map(|e| e.package.summary.as_deref()))
        .flatten()
        .map(str::to_lowercase)
        .collect();
    let key_equals = !normalised.is_empty() && keys.keys.iter().any(|k| k == normalised);
    let key_contains = !normalised.is_empty() && keys.keys.iter().any(|k| k.contains(normalised));

    let base = if names.iter().any(|n| n == query)
        || key_equals
        || keys.ids.iter().any(|id| id == query)
    {
        1.0
    } else if names.iter().any(|n| n.starts_with(query)) {
        0.9
    } else if key_contains || keys.ids.iter().any(|id| id.contains(query)) {
        0.75
    } else if summaries.iter().any(|s| {
        s.split(|c: char| !c.is_alphanumeric())
            .any(|w| w.starts_with(query))
    }) {
        0.6
    } else if summaries.iter().any(|s| s.contains(query)) {
        0.4
    } else {
        0.2
    };
    let mut score: f32 = base;
    if app.kind == PackageKind::App {
        score += 0.05;
    }
    if app.installed {
        score += 0.03;
    }
    if let Some(p) = app.popularity {
        score += (p.clamp(0.0, 1.0) * 0.1) as f32;
    }
    score
}

/// Two lone packages from one source with the same normalised name
/// (`yay`, `yay-bin`) would both be keyed `name:yay`. The page keys rows by
/// this, so every app in a clash gets its first edition's source and id
/// appended; only the clashing ones, so an unambiguous key stays short.
fn make_keys_unique(apps: &mut [App]) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for app in apps.iter() {
        *seen.entry(app.key.clone()).or_default() += 1;
    }
    for app in apps.iter_mut() {
        if seen.get(&app.key).copied().unwrap_or(0) > 1 {
            let first = &app.editions[0].package;
            app.key = format!("{}:{}:{}", app.key, first.source.id(), first.id);
        }
    }
    // A source that returned one id twice still clashes; number the rest.
    let mut used: HashMap<String, usize> = HashMap::new();
    for app in apps.iter_mut() {
        let n = used.entry(app.key.clone()).or_default();
        *n += 1;
        if *n > 1 {
            app.key = format!("{}:{}", app.key, n);
        }
    }
}

/// Disjoint sets over package indices, with path halving and the lower
/// index kept as root so a set's root is its first member. Each set
/// remembers two things about its members' AppStream ids, and a union that
/// would contradict either is refused: the catalogues have said these are
/// different applications, and no name match outranks that.
///
/// The first is the one reverse-DNS id the set carries, so GNOME's
/// `org.gnome.nautilus` and elementary's `io.elementary.files` never share
/// a row. The second is the id each source's members carry, whatever its
/// shape: the Arch catalogue gave `element` (an audio plugin host) and
/// `element-desktop` (the Matrix client) the ids `element` and
/// `io.element.Element`, and two sets that disagree on pacman's id are two
/// applications, even though the first id is not reverse-DNS and even when
/// an AUR build that answers to `element` would bridge them by name.
///
/// Keeping both on the set rather than checking the two packages of a pair
/// is what stops a third package with no id from bridging them.
struct Sets<'a> {
    parent: Vec<usize>,
    vendor_id: Vec<Option<&'a str>>,
    /// For each root, the AppStream id every source's members carry, one
    /// entry per (source, id). Two entries for one source cannot occur in
    /// a set: that is what `union` refuses.
    source_ids: Vec<Vec<(SourceKind, &'a str)>>,
}

impl<'a> Sets<'a> {
    fn new(facts: &'a [Facts]) -> Sets<'a> {
        let vendor_id = facts
            .iter()
            .map(|f| f.appstream_key.as_deref().filter(|id| is_reverse_dns(id)))
            .collect();
        let source_ids = facts
            .iter()
            .map(|f| {
                f.appstream_key
                    .as_deref()
                    .map(|id| vec![(f.source, id)])
                    .unwrap_or_default()
            })
            .collect();
        Sets {
            parent: (0..facts.len()).collect(),
            vendor_id,
            source_ids,
        }
    }

    /// A set per index with the given reverse-DNS ids and no per-source
    /// ids, for the unit tests.
    #[cfg(test)]
    fn with_ids(vendor_id: Vec<Option<&'a str>>) -> Sets<'a> {
        Sets {
            parent: (0..vendor_id.len()).collect(),
            source_ids: vec![Vec::new(); vendor_id.len()],
            vendor_id,
        }
    }

    /// A set per index with the given per-source ids and no reverse-DNS
    /// ids, for the unit tests.
    #[cfg(test)]
    fn with_source_ids(source_ids: Vec<Option<(SourceKind, &'a str)>>) -> Sets<'a> {
        Sets {
            parent: (0..source_ids.len()).collect(),
            vendor_id: vec![None; source_ids.len()],
            source_ids: source_ids
                .into_iter()
                .map(|id| id.into_iter().collect())
                .collect(),
        }
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    /// Join the two sets. Returns whether `a` and `b` are in one set
    /// afterwards, which is false only when the sets carry different
    /// reverse-DNS ids, or one source gave their members different ids.
    fn union(&mut self, a: usize, b: usize) -> bool {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return true;
        }
        let joined = match (self.vendor_id[ra], self.vendor_id[rb]) {
            (Some(x), Some(y)) if x != y => return false,
            (x, y) => x.or(y),
        };
        let disagree = self.source_ids[ra].iter().any(|(source, id)| {
            self.source_ids[rb]
                .iter()
                .any(|(other, other_id)| other == source && other_id != id)
        });
        if disagree {
            return false;
        }
        let (low, high) = if ra < rb { (ra, rb) } else { (rb, ra) };
        self.parent[high] = low;
        self.vendor_id[low] = joined;
        let mut moved = std::mem::take(&mut self.source_ids[high]);
        moved.retain(|entry| !self.source_ids[low].contains(entry));
        self.source_ids[low].append(&mut moved);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_lower_cases_and_drops_punctuation() {
        assert_eq!(normalise_key("Steam"), "steam");
        assert_eq!(normalise_key("  Visual Studio Code "), "visualstudiocode");
        assert_eq!(normalise_key("Steam (Runtime)"), "steamruntime");
        assert_eq!(normalise_key("gimp-plugin-gmic"), "gimpplugingmic");
    }

    #[test]
    fn key_reduces_a_reverse_dns_id_to_its_last_segment() {
        assert_eq!(normalise_key("com.valvesoftware.Steam"), "steam");
        assert_eq!(normalise_key("org.gimp.GIMP"), "gimp");
        assert_eq!(normalise_key("io.github.spillebulle.brokey"), "brokey");
        assert_eq!(normalise_key("com.valvesoftware.Steam.desktop"), "steam");
        assert_eq!(normalise_key("steam.desktop"), "steam");
        // Two segments is a host name, not an id; digits in the first
        // segment mean a version.
        assert_eq!(normalise_key("gimp.org"), "gimporg");
        assert_eq!(normalise_key("python-3.12.1"), "python3121");
        assert_eq!(normalise_key("Mr. Robot v.2"), "mrrobotv2");
    }

    #[test]
    fn key_keeps_the_vendor_when_the_last_segment_is_generic() {
        assert_eq!(normalise_key("org.telegram.desktop"), "telegramdesktop");
        assert_eq!(normalise_key("com.spotify.Client"), "spotifyclient");
        assert_eq!(normalise_key("telegram-desktop"), "telegramdesktop");
        // Only when the segment is on the list, and only for real ids: a
        // two-segment `.desktop` is a legacy desktop-file id.
        assert_eq!(normalise_key("org.gimp.GIMP"), "gimp");
        assert_eq!(normalise_key("com.example.thing"), "thing");
        assert_eq!(normalise_key("my.desktop"), "my");
    }

    #[test]
    fn key_strips_one_edition_suffix_when_a_base_remains() {
        assert_eq!(normalise_key("steam-git"), "steam");
        assert_eq!(normalise_key("yay-bin"), "yay");
        assert_eq!(normalise_key("firefox-nightly"), "firefox");
        assert_eq!(normalise_key("wine-stable"), "wine");
        assert_eq!(normalise_key("vscodium-electron"), "vscodium");
        assert_eq!(normalise_key("tilda-wayland"), "tilda");
        assert_eq!(normalise_key("kwin-x11"), "kwin");
        assert_eq!(normalise_key("obsidian-appimage"), "obsidian");
        // A release channel and a toolkit front-end are not editions.
        assert_eq!(
            normalise_key("telegram-desktop-beta"),
            "telegramdesktopbeta"
        );
        assert_eq!(normalise_key("signal-desktop-beta"), "signaldesktopbeta");
        assert_eq!(normalise_key("wireshark-qt"), "wiresharkqt");
        assert_eq!(normalise_key("transmission-gtk"), "transmissiongtk");
        assert_eq!(normalise_key("neovim-qt"), "neovimqt");
        // Only the last suffix goes.
        assert_eq!(normalise_key("brave-beta-bin"), "bravebeta");
        assert_eq!(normalise_key("fcitx5-qt-git"), "fcitx5qt");
        // Too little would remain.
        assert_eq!(normalise_key("go-bin"), "gobin");
        assert_eq!(normalise_key("qt-git"), "qtgit");
        assert_eq!(normalise_key("x-x11"), "xx11");
        // Not a suffix when it is the whole name or not at the end.
        assert_eq!(normalise_key("git"), "git");
        assert_eq!(normalise_key("gitg"), "gitg");
        assert_eq!(normalise_key("discord-canary"), "discordcanary");
        assert_eq!(normalise_key("steam-native-runtime"), "steamnativeruntime");
    }

    #[test]
    fn appstream_id_strips_desktop_and_reads_flatpak_refs() {
        let mut p = Package::new(SourceKind::Pacman, "steam", "steam");
        assert_eq!(appstream_id(&p), None);
        p.appstream_id = Some("com.valvesoftware.Steam.desktop".into());
        assert_eq!(appstream_id(&p).as_deref(), Some("com.valvesoftware.Steam"));
        p.appstream_id = Some("com.valvesoftware.Steam.DESKTOP".into());
        assert_eq!(appstream_id(&p).as_deref(), Some("com.valvesoftware.Steam"));
        p.appstream_id = Some("  ".into());
        assert_eq!(appstream_id(&p), None);
        // A real id whose last segment is "desktop" keeps it.
        p.appstream_id = Some("org.telegram.desktop".into());
        assert_eq!(appstream_id(&p).as_deref(), Some("org.telegram.desktop"));
        p.appstream_id = Some("org.telegram.desktop.desktop".into());
        assert_eq!(appstream_id(&p).as_deref(), Some("org.telegram.desktop"));

        let f = Package::new(
            SourceKind::Flatpak,
            "flathub/app/com.valvesoftware.Steam/x86_64/stable",
            "Steam",
        );
        assert_eq!(appstream_id(&f).as_deref(), Some("com.valvesoftware.Steam"));
        let bare = Package::new(SourceKind::Flatpak, "org.gimp.GIMP", "GIMP");
        assert_eq!(appstream_id(&bare).as_deref(), Some("org.gimp.GIMP"));
        let runtime = Package::new(
            SourceKind::Flatpak,
            "flathub/runtime/org.freedesktop.Platform/x86_64/24.08",
            "Freedesktop Platform",
        );
        assert_eq!(appstream_id(&runtime), None);
        // Only a Flatpak's id is an AppStream id.
        let snap = Package::new(SourceKind::Snap, "org.gimp.GIMP", "gimp");
        assert_eq!(appstream_id(&snap), None);
    }

    #[test]
    fn facts_key_on_name_package_name_and_appstream_id() {
        let mut p = Package::new(SourceKind::Pacman, "gimp", "GNU Image Manipulation Program");
        p.appstream_id = Some("org.gimp.GIMP".into());
        assert_eq!(
            Facts::of(&p).keys,
            vec!["gnuimagemanipulationprogram", "gimp"]
        );

        let runtime = Package::new(
            SourceKind::Flatpak,
            "flathub/runtime/org.freedesktop.Platform/x86_64/24.08",
            "Freedesktop Platform",
        );
        assert_eq!(
            Facts::of(&runtime).keys,
            vec!["freedesktopplatform", "platform"]
        );

        let gh = Package::new(SourceKind::Github, "sharkdp/bat", "bat");
        assert_eq!(Facts::of(&gh).keys, vec!["bat"]);

        let blank = Package::new(SourceKind::Aur, "---", "  ");
        assert_eq!(Facts::of(&blank).keys, vec!["aur:---"]);
    }

    #[test]
    fn significant_words_drop_short_and_stop_words() {
        let words = significant_words("Launcher for the Steam software distribution service");
        assert_eq!(words, vec!["distribution", "launcher", "service", "steam"]);
        let words = significant_words("A simple, fast tool with GTK.");
        assert!(words.is_empty(), "{words:?}");
        let gnu = significant_words("GNU Image Manipulation Program");
        let flathub = significant_words("High-end image creation and manipulation");
        assert_eq!(shared_words(&gnu, &flathub), 2);
    }

    #[test]
    fn every_false_friend_shares_its_key_and_is_known() {
        for (base, impostor) in FALSE_FRIENDS {
            assert_eq!(
                normalise_key(base),
                normalise_key(impostor),
                "{base} / {impostor} do not clash, so the list entry is dead"
            );
            assert_eq!(impostor_of(impostor), Some(*impostor));
            assert_eq!(impostor_of(base), None, "{base} is the real thing");
        }
        // An edition of a false friend is still that false friend.
        assert_eq!(
            impostor_of("wl-clipboard-x11-git"),
            Some("wl-clipboard-x11")
        );
        for edition in [
            "python-git",
            "perl-git",
            "nodejs-git",
            "rust-git",
            "yay-bin",
            "firefox-nightly",
            // No longer a clash at all, so no longer a false friend.
            "fcitx5-qt",
            "neovim-gtk",
        ] {
            assert_eq!(
                impostor_of(edition),
                None,
                "{edition} is an edition, not a false friend"
            );
        }
    }

    #[test]
    fn reverse_dns_is_three_segments_with_a_short_alphabetic_first() {
        assert!(is_reverse_dns("org.gnome.nautilus"));
        assert!(is_reverse_dns("io.github.spillebulle.brokey"));
        assert!(!is_reverse_dns("firefox"));
        assert!(!is_reverse_dns("gimp.org"));
        assert!(!is_reverse_dns("python-3.12.1"));
        assert!(!is_reverse_dns("org..broken"));
    }

    #[test]
    fn sets_keep_the_first_member_as_root() {
        let mut sets = Sets::with_ids(vec![None; 5]);
        assert!(sets.union(3, 1));
        assert!(sets.union(4, 3));
        assert_eq!(sets.find(4), 1);
        assert_eq!(sets.find(0), 0);
        assert_eq!(sets.find(2), 2);
        assert!(sets.union(2, 0));
        assert_eq!(sets.find(2), 0);
    }

    #[test]
    fn sets_refuse_to_join_two_different_vendor_ids_even_through_a_bridge() {
        let ids = vec![
            Some("org.gnome.nautilus"),
            Some("io.elementary.files"),
            None,
            None,
        ];
        let mut sets = Sets::with_ids(ids);
        assert!(!sets.union(0, 1), "different ids never join");
        assert!(sets.union(2, 0), "no id joins an id");
        let root = sets.find(2);
        assert_eq!(sets.vendor_id[root], Some("org.gnome.nautilus"));
        assert!(
            !sets.union(2, 1),
            "and having joined one, cannot join the other"
        );
        assert!(sets.union(3, 3), "a set is in one set with itself");
        let mut same = Sets::with_ids(vec![Some("org.gimp.gimp"), Some("org.gimp.gimp")]);
        assert!(same.union(0, 1), "the same id joins");
    }

    #[test]
    fn sets_refuse_to_join_when_one_source_gave_the_members_different_ids() {
        // pacman's element (a plain desktop-file id, so no vendor id) and
        // element-desktop, with an AUR build that answers to both by name.
        let mut sets = Sets::with_source_ids(vec![
            Some((SourceKind::Pacman, "element")),
            Some((SourceKind::Pacman, "io.element.element")),
            Some((SourceKind::Aur, "element")),
            None,
        ]);
        assert!(!sets.union(0, 1), "one source, two ids: two applications");
        assert!(sets.union(2, 0), "another source's id is no contradiction");
        assert!(
            !sets.union(2, 1),
            "and having joined one, the AUR build cannot bridge to the other"
        );
        assert!(sets.union(3, 1), "no id joins anything");
        assert!(!sets.union(3, 0), "but carries its set's ids with it");
        let root = sets.find(3);
        assert_eq!(
            sets.source_ids[root],
            vec![(SourceKind::Pacman, "io.element.element")]
        );
        // The same id from one source twice is one entry, not a clash.
        let mut same = Sets::with_source_ids(vec![
            Some((SourceKind::Pacman, "org.gnu.emacs")),
            Some((SourceKind::Pacman, "org.gnu.emacs")),
        ]);
        assert!(same.union(0, 1));
        assert_eq!(same.source_ids[0].len(), 1);
    }
}
