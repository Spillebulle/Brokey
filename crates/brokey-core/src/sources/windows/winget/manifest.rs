//! The locale manifest: where a winget package says what it is.
//!
//! The catalogue in [`super::index`] is a search index, and only that: its
//! eleven tables were counted when the Windows design was written, and
//! `docs/superpowers/specs/2026-09-19-windows-support-design.md` records that
//! none of them has a description column or an icon column. So a package
//! found there has a name, a version and nothing to read. The `winget-pkgs`
//! repository holds a manifest per package per version, and its address is
//! derivable from the id and the version the index already has:
//! [`manifest_url`]. [`parse`] reads one.
//!
//! **This is not a general YAML reader, and it must not be handed a file
//! that needs one.** These manifests are a narrow, machine-generated subset,
//! and taking a YAML dependency to read eight scalars and a list of tags is
//! not worth it. What is handled is exactly this:
//!
//! - `Key: value` at the left margin, with the key a word of ASCII letters
//!   and digits. Anything else before the first colon is not a key.
//! - A quoted value, which is what the emitter writes when a value carries a
//!   colon or begins with a character a plain scalar cannot. Single quotes
//!   take `''` for an apostrophe; double quotes are unescaped as far as
//!   `\\`, `\"`, `\n` and `\t`.
//! - A block scalar opened by `Key: |` or `Key: >`, with the chomping and
//!   indentation indicators (`-`, `+`, a digit) accepted. Its indented lines
//!   follow, the indentation of the first is taken off all of them, and
//!   trailing blank lines are dropped. `|` keeps its line breaks and `>`
//!   folds them to spaces, a blank line becoming a break.
//! - A sequence of `- item` lines under a bare `Key:`.
//! - `#` comment lines and blank lines at the left margin.
//! - CRLF: a trailing carriage return comes off every line before any of the
//!   above looks at it.
//!
//! Everything else is ignored rather than guessed at, and the reader never
//! fails: [`parse`] always answers a [`Described`], filling what it
//! recognised and leaving the rest empty. In particular it does **not**
//! handle a nested mapping or a sequence of mappings (`Documentations:` is
//! one, and is skipped whole rather than half read), flow style (`[a, b]`,
//! `{a: b}`), anchors, aliases, tags, `---` document markers, or a plain
//! scalar continued on a second line. Two smaller departures from YAML are
//! deliberate: a `#` after a value on the same line is part of the value, so
//! that a URL keeps its fragment, and a folded scalar does not implement the
//! rule that a more-indented line keeps its own breaks.
//!
//! Where it is used: [`super::Winget`]'s `details` only, never its `search`.
//! It is one HTTP fetch per package, and a search page of twenty results
//! cannot afford twenty of them. The fetch goes through
//! `crate::http::Client`'s disk cache the way the catalogue does.

use crate::model::Package;

/// What a locale manifest says about a package.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Described {
    pub publisher: Option<String>,
    pub short_description: Option<String>,
    pub description: Option<String>,
    pub publisher_url: Option<String>,
    pub package_url: Option<String>,
    pub license: Option<String>,
    pub license_url: Option<String>,
    pub release_notes_url: Option<String>,
    pub tags: Vec<String>,
}

/// Where the manifests live. `master` rather than a tag, because the
/// repository has no tags and a package's manifest is only ever added to it.
const MANIFESTS: &str = "https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests";

/// Where `winget-pkgs` keeps the English manifest for one version.
///
/// The first segment is the lowercased first character of the id, then the id
/// split on `.` with each part its own directory, then the version, then the
/// id followed by `.locale.en-US.yaml`. `GIMP.GIMP` at `3.2.4` is at
/// `manifests/g/GIMP/GIMP/3.2.4/GIMP.GIMP.locale.en-US.yaml`, and
/// `Microsoft.VisualStudio.2022.Community` really does become four
/// directories.
///
/// `None` rather than a guess when the id or the version cannot be a path:
/// both are interpolated into an address, so a part that is empty, that
/// starts with something other than a letter or a digit, or that carries a
/// character outside the set below would fetch some other file from the same
/// repository, or nothing at all.
pub fn manifest_url(id: &str, version: &str) -> Option<String> {
    let parts: Vec<&str> = id.split('.').collect();
    if parts.len() < 2 || !parts.iter().all(|p| is_path_safe(p)) || !is_path_safe(version) {
        return None;
    }
    let first = id.chars().next()?.to_ascii_lowercase();
    Some(format!(
        "{MANIFESTS}/{first}/{}/{version}/{id}.locale.en-US.yaml",
        parts.join("/")
    ))
}

/// One segment of the address above: it begins with an ASCII letter or digit
/// and carries nothing but those and `-`, `_`, `.`, `+` and `~`, which are
/// the characters a package id and a version are made of and are all
/// unreserved in a URL. This is what refuses `..`, a leading dot, a slash and
/// anything that would need escaping.
fn is_path_safe(segment: &str) -> bool {
    segment.starts_with(|c: char| c.is_ascii_alphanumeric())
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '~'))
}

/// How a block scalar treats the line breaks inside it.
#[derive(Clone, Copy)]
enum Block {
    /// `|`: every line break is kept.
    Literal,
    /// `>`: a line break becomes a space and a blank line becomes a break.
    Folded,
}

/// Read one. Not a general YAML reader: see the module's doc comment.
pub fn parse(yaml: &str) -> Described {
    // A trailing carriage return is dropped here, once, so that nothing below
    // has to think about it. The manifests in `winget-pkgs` are CRLF, and a
    // reader that split on `\n` alone would leave a `\r` on the end of every
    // value and every tag.
    let lines: Vec<&str> = yaml
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();

    let mut d = Described::default();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        // Only a key at the left margin opens anything. A blank line, a
        // comment and an indented line that no key above claimed all mean
        // there is nothing here to read.
        let Some((key, rest)) = split_key(line) else {
            continue;
        };
        if rest.is_empty() {
            // A bare key: either a sequence under it, or a key with no value
            // at all, which reads as an empty sequence and fills nothing.
            let items = read_sequence(&lines, &mut i);
            if let Some(items) = items {
                set_sequence(&mut d, key, items);
            }
        } else if let Some(block) = block_style(rest) {
            set_scalar(&mut d, key, read_block(&lines, &mut i, block));
        } else {
            set_scalar(&mut d, key, scalar(rest));
        }
    }
    d
}

/// `Key: rest` at the left margin, with the key a word of ASCII letters and
/// digits, which is every key this schema writes. Everything else is `None`:
/// a blank line, a `#` comment, an indented line, a `- ` item that no
/// sequence above claimed, and any line with no colon on it.
fn split_key(line: &str) -> Option<(&str, &str)> {
    let (key, rest) = line.split_once(':')?;
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((key, rest.trim()))
}

/// `|`, `>` and their chomping and indentation indicators (`|-`, `>-`, `|+`,
/// `|2`), which is what stands after the colon when a block scalar follows.
fn block_style(rest: &str) -> Option<Block> {
    let (first, indicators) = rest.split_at_checked(1)?;
    if !indicators
        .chars()
        .all(|c| matches!(c, '-' | '+') || c.is_ascii_digit())
    {
        return None;
    }
    match first {
        "|" => Some(Block::Literal),
        ">" => Some(Block::Folded),
        _ => None,
    }
}

/// The indented lines under a block scalar, with the indentation of the first
/// of them taken off all of them, and trailing blank lines dropped. The
/// chomping and indentation indicators are accepted and then ignored: the
/// indentation comes from the first line rather than from a `|2`, and a value
/// that ends in blank lines is not something a description wants.
fn read_block(lines: &[&str], i: &mut usize, block: Block) -> String {
    let mut body: Vec<&str> = Vec::new();
    let mut indent: Option<usize> = None;
    while *i < lines.len() {
        let line = lines[*i];
        if line.is_empty() {
            body.push("");
            *i += 1;
            continue;
        }
        let here = line.len() - line.trim_start().len();
        if here == 0 {
            break;
        }
        *i += 1;
        let take = *indent.get_or_insert(here);
        body.push(&line[take.min(here)..]);
    }
    while body.last().is_some_and(|l| l.is_empty()) {
        body.pop();
    }
    match block {
        Block::Literal => body.join("\n"),
        Block::Folded => fold(&body),
    }
}

/// YAML's folding, as far as this reader goes: a line break becomes a space
/// and a blank line becomes a break. The rule that a more-indented line keeps
/// its own breaks is not implemented.
fn fold(body: &[&str]) -> String {
    let mut out = String::new();
    for line in body {
        if line.is_empty() {
            out.push('\n');
        } else {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push(' ');
            }
            out.push_str(line);
        }
    }
    out
}

/// The `- item` lines under a bare key. `None` when any of them is a mapping
/// (`- Key: value`, with its own indented lines under it), because a sequence
/// of mappings is a shape this reader does not handle: it is skipped whole
/// rather than half read into a list of strings.
fn read_sequence(lines: &[&str], i: &mut usize) -> Option<Vec<String>> {
    let mut items = Vec::new();
    let mut nested = false;
    while *i < lines.len() {
        let line = lines[*i];
        if line.is_empty() {
            *i += 1;
            continue;
        }
        if let Some(item) = line.strip_prefix("- ") {
            *i += 1;
            let item = item.trim();
            // `- Key: value` is the first line of a mapping, not a string.
            if item.contains(": ") || item.ends_with(':') {
                nested = true;
            }
            items.push(scalar(item));
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // An indented line under an item: the item is a mapping and this
            // is the rest of it.
            *i += 1;
            nested = true;
            continue;
        }
        // A line at the left margin: the sequence has ended.
        break;
    }
    if nested { None } else { Some(items) }
}

/// A scalar with its quotes taken off. A single-quoted scalar takes an
/// apostrophe as two of them and treats everything else literally; a
/// double-quoted one is unescaped as far as `\\`, `\"`, `\n` and `\t`, and
/// any other escape is kept as written. A `#` after a plain value is part of
/// it rather than a comment, which keeps the fragment on a URL.
fn scalar(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        if let Some(inner) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            return inner.replace("''", "'");
        }
        if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            return unescape(inner);
        }
    }
    value.to_string()
}

fn unescape(inner: &str) -> String {
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn set_scalar(d: &mut Described, key: &str, value: String) {
    let value = if value.is_empty() { None } else { Some(value) };
    match key {
        "Publisher" => d.publisher = value,
        "ShortDescription" => d.short_description = value,
        "Description" => d.description = value,
        "PublisherUrl" => d.publisher_url = value,
        "PackageUrl" => d.package_url = value,
        "License" => d.license = value,
        "LicenseUrl" => d.license_url = value,
        "ReleaseNotesUrl" => d.release_notes_url = value,
        // Every other key in the schema is something this reader has no field
        // for, and is ignored rather than guessed at.
        _ => {}
    }
}

fn set_sequence(d: &mut Described, key: &str, items: Vec<String>) {
    if key == "Tags" {
        d.tags = items;
    }
}

/// Fold what the locale manifest says into the package the index built.
///
/// This is where the spelling changes, and the only place it does: the
/// manifest's key is `License` and [`Described`] keeps that, because the
/// struct is a reading of a document whose key really is spelt so. `Package`
/// is Brokey's own, and Brokey writes `licence`.
///
/// The homepage is the package's own page where the manifest has one and the
/// publisher's where it does not, which is the order a user would want them
/// in. A field the manifest leaves out leaves the package as the index
/// described it, so this never replaces a value with nothing.
pub fn describe(p: &mut Package, d: &Described) {
    if let Some(v) = &d.short_description {
        p.summary = Some(v.clone());
    }
    if let Some(v) = &d.description {
        p.description = Some(v.clone());
    }
    if let Some(v) = &d.license {
        p.licence = Some(v.clone());
    }
    if let Some(v) = d.package_url.as_ref().or(d.publisher_url.as_ref()) {
        p.homepage = Some(v.clone());
    }
    if let Some(v) = &d.publisher {
        p.developer = Some(v.clone());
    }
    if !d.tags.is_empty() {
        p.categories = d.tags.clone();
    }
    if let Some(v) = &d.license_url {
        p.facts.push(("Licence URL".to_string(), v.clone()));
    }
    if let Some(v) = &d.release_notes_url {
        p.facts.push(("Release notes".to_string(), v.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/winget")
            .join(name);
        std::fs::read_to_string(&path).expect("the fixture is checked in")
    }

    /// The four shapes were fetched and checked before this was written, so
    /// the strings below are addresses that answer 200 rather than addresses
    /// derived from the rule a second time.
    #[test]
    fn the_path_comes_from_the_id_and_the_version() {
        assert_eq!(
            manifest_url("GIMP.GIMP", "3.2.4").as_deref(),
            Some(
                "https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests/g/GIMP/GIMP/3.2.4/GIMP.GIMP.locale.en-US.yaml"
            )
        );
        assert_eq!(
            manifest_url("Microsoft.VisualStudio.2022.Community", "17.10.0").as_deref(),
            Some(
                "https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests/m/Microsoft/VisualStudio/2022/Community/17.10.0/Microsoft.VisualStudio.2022.Community.locale.en-US.yaml"
            )
        );
        assert_eq!(
            manifest_url("7zip.7zip", "24.09").as_deref(),
            Some(
                "https://raw.githubusercontent.com/microsoft/winget-pkgs/master/manifests/7/7zip/7zip/24.09/7zip.7zip.locale.en-US.yaml"
            )
        );
    }

    #[test]
    fn an_id_that_cannot_be_a_path_gives_nothing() {
        assert_eq!(manifest_url("", "1.0"), None);
        assert_eq!(manifest_url(".x", "1.0"), None);
        assert_eq!(manifest_url("nodot", "1.0"), None);
        assert_eq!(manifest_url("-leading.Dash", "1.0"), None);
        assert_eq!(manifest_url("Two..Dots", "1.0"), None);
        assert_eq!(manifest_url("Has/Slash.Inside", "1.0"), None);
        assert_eq!(manifest_url("Trailing.", "1.0"), None);
    }

    /// The version is interpolated into the same address, so it is held to
    /// the same rule. Without this a version of `..` climbs a directory and
    /// fetches some other package's manifest.
    #[test]
    fn a_version_that_cannot_be_a_path_gives_nothing() {
        assert_eq!(manifest_url("GIMP.GIMP", ""), None);
        assert_eq!(manifest_url("GIMP.GIMP", ".."), None);
        assert_eq!(manifest_url("GIMP.GIMP", "3.2.4/../../evil"), None);
        assert_eq!(manifest_url("GIMP.GIMP", "2.0 Beta"), None);
        // The shapes a real version takes are all still paths.
        assert!(manifest_url("GIMP.GIMP", "1.0.0-beta.2").is_some());
        assert!(manifest_url("GIMP.GIMP", "2021.1.2+build").is_some());
    }

    #[test]
    fn a_real_manifest_reads() {
        let d = parse(&fixture("gimp.locale.yaml"));
        assert_eq!(
            d,
            Described {
                publisher: Some("The GIMP Team".to_string()),
                short_description: Some(
                    "GIMP is an acronym for GNU Image Manipulation Program. It is a freely \
                     distributed program for such tasks as photo retouching, image composition \
                     and image authoring."
                        .to_string()
                ),
                // GIMP's manifest carries no `Description` at all, only the
                // short one.
                description: None,
                publisher_url: Some("https://www.gimp.org/".to_string()),
                package_url: Some("https://www.gimp.org/downloads/".to_string()),
                license: Some("GPLv3".to_string()),
                license_url: Some("https://www.gimp.org/about/COPYING".to_string()),
                release_notes_url: Some(
                    "https://www.gimp.org/news/2026/04/19/gimp-3-2-4-released/".to_string()
                ),
                tags: [
                    "gnu-image-manipulation-program",
                    "gnuimagemanipulationprogram",
                    "image-editor",
                    "image-editing",
                    "imageediting",
                    "paint",
                    "painting",
                    "photoediting",
                    "photo-editing",
                    "pictures",
                    "9pnsjclxdz0v",
                ]
                .map(str::to_string)
                .to_vec(),
            }
        );
    }

    #[test]
    fn a_block_scalar_keeps_its_line_breaks_and_loses_its_indentation() {
        let d = parse(&fixture("blender.locale.yaml"));
        let description = d.description.expect("Blender's manifest has a description");
        assert_eq!(description.lines().count(), 3, "{description}");
        for line in description.lines() {
            assert!(!line.starts_with(' '), "indented: {line}");
        }
        assert!(
            description.starts_with("Welcome to Blender!"),
            "{description}"
        );
        assert!(
            description.ends_with("hardware and platforms."),
            "{description}"
        );
    }

    /// `Documentations:` is a sequence of mappings, which this reader does
    /// not handle. It must not leak into `tags`, and it must not leak into a
    /// scalar either: `DocumentUrl` is indented under its item and a reader
    /// that took indented lines for keys would pick it up.
    #[test]
    fn a_nested_sequence_this_reader_does_not_handle_is_skipped_whole() {
        let d = parse(&fixture("blender.locale.yaml"));
        assert_eq!(
            d.tags,
            ["3d", "animation", "model", "modeling", "render", "vfx"]
        );
        assert_eq!(d.publisher_url, None);
        assert_eq!(d.package_url, None);
        assert_eq!(d.release_notes_url, None);
        let joined = format!("{d:?}");
        assert!(!joined.contains("DocumentLabel"), "{joined}");
        assert!(!joined.contains("docs.blender.org"), "{joined}");
    }

    /// The same shape under a key this reader does read. Blender's
    /// `Documentations:` is skipped because the key is unknown as well as
    /// because the shape is, so on its own it cannot show which of the two
    /// did the skipping. Here it is `Tags:`, and the list must come back
    /// empty rather than carrying `DocumentLabel: Manual` as a tag.
    #[test]
    fn a_sequence_of_mappings_under_a_key_this_reader_reads_is_skipped_whole() {
        let d = parse(
            "Tags:\n\
             - DocumentLabel: Manual\n  \
             DocumentUrl: https://example.com/manual\n\
             License: MIT\n",
        );
        assert!(d.tags.is_empty(), "{:?}", d.tags);
        // The key after the block is still read, so the skip took exactly
        // the mapping and not the rest of the file.
        assert_eq!(d.license.as_deref(), Some("MIT"));
    }

    /// An indented line that no key above claimed is not a key. Trimming
    /// before the colon would make `DocumentUrl:` inside a mapping into a
    /// key of its own, which is how a nested shape leaks into a scalar.
    #[test]
    fn an_indented_line_no_key_claimed_is_not_a_key() {
        let d = parse("License: MIT\n  LicenseUrl: https://example.com/stray\n");
        assert_eq!(d.license.as_deref(), Some("MIT"));
        assert_eq!(d.license_url, None);
    }

    /// An empty quoted scalar and a block scalar with nothing under it are
    /// both nothing rather than an empty string. An empty string would draw
    /// a row on the detail page with no text in it.
    #[test]
    fn an_empty_value_is_nothing_rather_than_an_empty_string() {
        let d = parse("License: ''\nDescription: |-\nTags:\n- one\n");
        assert_eq!(d.license, None);
        assert_eq!(d.description, None);
        assert_eq!(d.tags, ["one"]);
    }

    /// A block scalar at the end of a file, with a blank line after it: the
    /// value must not keep the blank line. A description ending in white
    /// space draws a gap on the detail page that nobody wrote.
    #[test]
    fn a_block_scalar_keeps_no_trailing_blank_line() {
        let d = parse("Description: |\n  One line\n\n\n");
        assert_eq!(d.description.as_deref(), Some("One line"));
    }

    #[test]
    fn a_manifest_missing_everything_optional_is_not_an_error() {
        let d = parse("PackageIdentifier: Some.Thing\nPackageVersion: 1.0\nLicense: MIT\n");
        assert_eq!(
            d,
            Described {
                license: Some("MIT".to_string()),
                ..Described::default()
            }
        );
        assert_eq!(parse(""), Described::default());
    }

    #[test]
    fn a_key_this_reader_does_not_know_is_ignored() {
        let d = parse(
            "# a comment\n\
             Copyright: 2026 Somebody\n\
             InstallationNotes: Run it from the Start menu\n\
             PurchaseUrl: https://example.com/buy\n\
             License: MIT\n",
        );
        assert_eq!(
            d,
            Described {
                license: Some("MIT".to_string()),
                ..Described::default()
            }
        );
    }

    /// The manifests in `winget-pkgs` are CRLF, and this is pinned inline
    /// rather than against a fixture because `.gitattributes` normalises a
    /// checked-in file to LF: the fixture would prove this today and stop
    /// proving it on the next fresh clone.
    #[test]
    fn crlf_line_endings_do_not_end_up_in_the_values() {
        let d = parse(
            "# a comment\r\n\
             \r\n\
             Publisher: A Team\r\n\
             ShortDescription: One short line\r\n\
             Description: |-\r\n  \
             First line\r\n  \
             Second line\r\n\
             License: MIT\r\n\
             LicenseUrl: https://example.com/COPYING\r\n\
             Tags:\r\n\
             - one\r\n\
             - two\r\n",
        );
        assert_eq!(d.publisher.as_deref(), Some("A Team"));
        assert_eq!(d.short_description.as_deref(), Some("One short line"));
        assert_eq!(d.description.as_deref(), Some("First line\nSecond line"));
        assert_eq!(d.license.as_deref(), Some("MIT"));
        assert_eq!(
            d.license_url.as_deref(),
            Some("https://example.com/COPYING")
        );
        assert_eq!(d.tags, ["one", "two"]);
        let every = [
            d.publisher.clone(),
            d.short_description.clone(),
            d.description.clone(),
            d.license.clone(),
            d.license_url.clone(),
        ];
        for value in every.iter().flatten().chain(d.tags.iter()) {
            assert!(!value.contains('\r'), "carriage return in {value:?}");
        }
    }

    /// A folded scalar is not a literal one: its line breaks become spaces.
    /// Reading `>` the way `|` is read would put a break in the middle of a
    /// sentence on the detail page.
    #[test]
    fn a_folded_block_scalar_joins_its_lines() {
        let d = parse("Description: >-\n  First half\n  second half\n\n  New paragraph\n");
        assert_eq!(
            d.description.as_deref(),
            Some("First half second half\nNew paragraph")
        );
    }

    /// A value the emitter had to quote, because it carries a colon or begins
    /// with a character a plain scalar cannot begin with. The quotes are the
    /// emitter's, not the publisher's, and showing them to a user would be
    /// showing them YAML.
    #[test]
    fn a_quoted_scalar_loses_its_quotes() {
        let d = parse(
            "ShortDescription: 'Notepad: a plain editor'\n\
             License: \"MIT\"\n\
             Publisher: 'Bob''s Software'\n\
             PackageUrl: \"https://example.com/a\\\\b\"\n",
        );
        assert_eq!(
            d.short_description.as_deref(),
            Some("Notepad: a plain editor")
        );
        assert_eq!(d.license.as_deref(), Some("MIT"));
        assert_eq!(d.publisher.as_deref(), Some("Bob's Software"));
        assert_eq!(d.package_url.as_deref(), Some("https://example.com/a\\b"));
    }

    /// Inside a block scalar a `#` is text. A reader that looked for comments
    /// before it looked at the block it is in would cut a description in
    /// half at the first one.
    #[test]
    fn a_hash_inside_a_block_scalar_is_content() {
        let d = parse("Description: |-\n  Supports C# and F#.\n  # not a comment\nLicense: MIT\n");
        assert_eq!(
            d.description.as_deref(),
            Some("Supports C# and F#.\n# not a comment")
        );
        assert_eq!(d.license.as_deref(), Some("MIT"));
    }

    /// A `#` after a plain value is kept, so a URL keeps its fragment.
    #[test]
    fn a_hash_after_a_value_is_part_of_it() {
        let d = parse("ReleaseNotesUrl: https://example.com/notes#latest\n");
        assert_eq!(
            d.release_notes_url.as_deref(),
            Some("https://example.com/notes#latest")
        );
    }

    /// The block ends at the first line back at the left margin, and the key
    /// on that line is read as a key rather than swallowed.
    #[test]
    fn a_key_after_a_block_scalar_is_still_read() {
        let d = parse("Description: |-\n  A line\nLicense: MIT\nTags:\n- one\n");
        assert_eq!(d.description.as_deref(), Some("A line"));
        assert_eq!(d.license.as_deref(), Some("MIT"));
        assert_eq!(d.tags, ["one"]);
    }

    /// What `details` does with what it read. The homepage is the package's
    /// own page, the licence crosses the spelling, and the facts are in the
    /// order they are drawn in.
    #[test]
    fn a_package_takes_the_manifests_word_for_it() {
        let d = parse(&fixture("gimp.locale.yaml"));
        let mut p =
            crate::model::Package::new(crate::model::SourceKind::Winget, "GIMP.GIMP", "GIMP");
        describe(&mut p, &d);
        assert_eq!(p.summary.as_deref().map(|s| &s[..4]), Some("GIMP"));
        assert_eq!(p.description, None);
        assert_eq!(p.licence.as_deref(), Some("GPLv3"));
        assert_eq!(
            p.homepage.as_deref(),
            Some("https://www.gimp.org/downloads/")
        );
        assert_eq!(p.developer.as_deref(), Some("The GIMP Team"));
        assert_eq!(p.categories.len(), 11);
        assert_eq!(
            p.facts,
            vec![
                (
                    "Licence URL".to_string(),
                    "https://www.gimp.org/about/COPYING".to_string()
                ),
                (
                    "Release notes".to_string(),
                    "https://www.gimp.org/news/2026/04/19/gimp-3-2-4-released/".to_string()
                ),
            ]
        );
    }

    /// Blender's manifest has no `PackageUrl`, so the publisher's page is the
    /// homepage. Without the fallback the detail page would have no link at
    /// all for a package whose manifest names one.
    #[test]
    fn the_homepage_falls_back_to_the_publisher_url() {
        let d = Described {
            publisher_url: Some("https://example.com/publisher".to_string()),
            ..Described::default()
        };
        let mut p = crate::model::Package::new(crate::model::SourceKind::Winget, "A.B", "B");
        describe(&mut p, &d);
        assert_eq!(p.homepage.as_deref(), Some("https://example.com/publisher"));
    }

    /// A manifest that says nothing leaves every value the index filled
    /// exactly as it was, rather than replacing it with nothing.
    #[test]
    fn an_empty_manifest_replaces_nothing() {
        let mut p = crate::model::Package::new(crate::model::SourceKind::Winget, "A.B", "B");
        p.summary = Some("From the index".to_string());
        p.homepage = Some("https://example.com/".to_string());
        p.categories = vec!["kept".to_string()];
        let before = p.clone();
        describe(&mut p, &Described::default());
        assert_eq!(p, before);
    }
}
