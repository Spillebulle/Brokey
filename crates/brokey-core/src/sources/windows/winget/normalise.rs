//! winget's own name and publisher normaliser, ported, so that a registry
//! `DisplayName` folds to the value the catalogue stored for it.
//!
//! The catalogue's `norm_names2` and `norm_publishers2` were written by
//! `NameNormalization.cpp` in `microsoft/winget-cli` (the `Initial`
//! normaliser, the only one it has). It does much more than fold to letters
//! and digits: it strips architectures, locale tags, versions, bracketed
//! asides and legal-entity suffixes, which is why it stored `python` for
//! `Python 3.0` and `mozillafirefox` for `Mozilla Firefox (en-US)`. A
//! registry `DisplayName` almost always carries a version, so without this
//! the name rung of the registry join compared `advancedipscanner251` with
//! `advancedipscanner` and missed.
//!
//! The order of the steps and every pattern are winget's. The patterns use
//! look-around, which the `regex` crate does not have, so each look-behind is
//! a check on the character before a candidate match, retried one character
//! further on when it fails, which is what the engine winget uses does. Three
//! of winget's patterns are left out because they name single products
//! (`Roblox`, `Bomgar`) or a format nothing on Windows writes into an
//! uninstall key (SAP's package names); each would only ever make a name
//! that already misses miss differently.
//!
//! The output is folded to lower case, as the catalogue stores it. The
//! architecture is not part of the name: the catalogue writes it after the
//! name in brackets (`python(X64)`), and [`architecture_suffixes`] gives the
//! forms a lookup should accept.

use regex::Regex;
use std::sync::LazyLock;

/// The words winget counts as a version marker in front of a number.
const VERSION_WORDS: &str = r"V|VER|VERSI(?:O|Ó)N|VERSÃO|VERSIE|WERSJA|BUILD|RELEASE|RC|SP";

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a normaliser pattern compiles")
}

struct Patterns {
    arch_32_or_64: Regex,
    arch_x64: Regex,
    arch_64_bit: Regex,
    arch_x32: Regex,
    arch_32_bit: Regex,
    arch_arm64: Regex,
    arch_arm: Regex,
    locale: Regex,
    kb: Regex,
    prefix_parens: Regex,
    empty_parens: Regex,
    file_path_ghs: Regex,
    file_path_parens: Regex,
    file_path_quotes: Regex,
    file_path: Regex,
    version_letter: Regex,
    version_delimited: Regex,
    version_word_before: Regex,
    version: Regex,
    en: Regex,
    non_nested_bracket: Regex,
    bracket_enclosed: Regex,
    uri_protocol: Regex,
    leading_symbols: Regex,
    trailing_symbols: Regex,
    trailing_non_letters: Regex,
}

static P: LazyLock<Patterns> = LazyLock::new(|| Patterns {
    // The architecture tokens without winget's leading look-behind
    // (`(?<=^|[^\p{L}\p{Nd}])`) and trailing look-ahead (`(?=\P{Nd}|$)`),
    // which `remove_architecture` checks by hand.
    arch_32_or_64: re(r"(?i)(?:64[\\/]32|32[\\/]64)[\p{Pd}\p{Pc}\p{Z}]?BITS?(?:\sEDITION)?"),
    arch_x64: re(r"(?i)(?:X64|AMD64|X86[\p{Pd}\p{Pc}]64)(?:\sEDITION)?"),
    arch_64_bit: re(r"(?i)64[\p{Pd}\p{Pc}\p{Z}]?BITS?(?:\sEDITION)?"),
    arch_x32: re(r"(?i)(?:X32|X86)(?:\sEDITION)?"),
    arch_32_bit: re(r"(?i)32[\p{Pd}\p{Pc}\p{Z}]?BITS?(?:\sEDITION)?"),
    arch_arm64: re(r"(?i)(?:ARM64|AARCH64)(?:\sEDITION)?"),
    arch_arm: re(r"(?i)ARM32(?:\sEDITION)?"),
    // Two or three letters, an optional script, a region: `en-US`,
    // `sr-Latn-RS`. `remove_locales` adds `-valencia` and removes only
    // those in [`LOCALES`].
    locale: re(r"(?i)\p{L}{2,3}(?:-(?:CANS|CYRL|LATN|MONG))?-\p{L}{2}"),
    kb: re(r"(?i)\((KB\d+)\)"),
    prefix_parens: re(r"^\(.*?\)"),
    empty_parens: re(r#"\(\s*\)|\[\s*\]|"\s*""#),
    file_path_ghs: re(r"(?i)\(CHANGE #\d{1,2} TO [CDEF]:\\(?:.+?\\)*[^\s]*\\?\)"),
    file_path_parens: re(r"(?i)\([CDEF]:\\(?:.+?\\)*[^\s]*\\?\)"),
    file_path_quotes: re(r#"(?i)"[CDEF]:\\(?:.+?\\)*[^\s]*\\?""#),
    file_path: re(r"(?i)(?:(?:INSTALLED\sAT|IN)\s)?[CDEF]:\\(?:.+?\\)*[^\s]*\\?"),
    version_letter: re(&format!(
        r"(?i)(?:(?:{VERSION_WORDS})\P{{L}})?\p{{L}}\p{{Nd}}+(?:[\p{{Po}}\p{{Pd}}\p{{Pc}}]\p{{Nd}}+)+"
    )),
    version_delimited: re(
        r"(?i)\p{Nd}+(?:[\p{Po}\p{Pd}\p{Pc}]\p{Nd}?(?:RC|B|A|R|SP|K)?\p{Nd}+)+(?:[\p{Po}\p{Pd}\p{Pc}]?[\p{L}\p{Nd}]+)*",
    ),
    version_word_before: re(&format!(r"(?i)(?:{VERSION_WORDS})\P{{L}}?$")),
    version: re(&format!(
        r"(?i)(FOR\s)?(?:P|{VERSION_WORDS}|R)(?:\P{{L}}|\P{{L}}\p{{L}})?(?:\p{{Nd}}|\.\p{{Nd}})+(?:RC|B|A|R|V|SP)?\p{{Nd}}?"
    )),
    en: re(r"(?i)\sEN\s*$"),
    non_nested_bracket: re(r"\([^()]*\)|\[[^\[\]]*\]"),
    bracket_enclosed: re(r#"\p{Ps}.*\p{Pe}|".*""#),
    uri_protocol: re(r"(?i)(?:https?|ftp)://"),
    leading_symbols: re(r"^[^\p{L}\p{Nd}]+"),
    trailing_symbols: re(r"[^\p{L}\p{Nd}]+$"),
    trailing_non_letters: re(r"\P{L}+$"),
});

/// The locales winget removes from a name. Anything else shaped like one
/// (`ab-CD`) stays, because it may be part of the name.
const LOCALES: &[&str] = &[
    "af-za",
    "am-et",
    "ar-ae",
    "ar-bh",
    "ar-dz",
    "ar-eg",
    "ar-iq",
    "ar-jo",
    "ar-kw",
    "ar-lb",
    "ar-ly",
    "ar-ma",
    "arn-cl",
    "ar-om",
    "ar-qa",
    "ar-sa",
    "ar-sy",
    "ar-tn",
    "ar-ye",
    "as-in",
    "ba-ru",
    "be-by",
    "bg-bg",
    "bn-bd",
    "bn-in",
    "bo-cn",
    "br-fr",
    "ca-es",
    "ca-es-valencia",
    "co-fr",
    "cs-cz",
    "cy-gb",
    "da-dk",
    "de-at",
    "de-ch",
    "de-de",
    "de-li",
    "de-lu",
    "dsb-de",
    "dv-mv",
    "el-gr",
    "en-au",
    "en-bz",
    "en-ca",
    "en-gb",
    "en-ie",
    "en-in",
    "en-jm",
    "en-my",
    "en-nz",
    "en-ph",
    "en-sg",
    "en-tt",
    "en-us",
    "en-za",
    "en-zw",
    "es-ar",
    "es-bo",
    "es-cl",
    "es-co",
    "es-cr",
    "es-do",
    "es-ec",
    "es-es",
    "es-gt",
    "es-hn",
    "es-mx",
    "es-ni",
    "es-pa",
    "es-pe",
    "es-pr",
    "es-py",
    "es-sv",
    "es-us",
    "es-uy",
    "es-ve",
    "et-ee",
    "eu-es",
    "fa-ir",
    "fi-fi",
    "fil-ph",
    "fo-fo",
    "fr-be",
    "fr-ca",
    "fr-ch",
    "fr-fr",
    "fr-lu",
    "fr-mc",
    "fy-nl",
    "ga-ie",
    "gd-db",
    "gl-es",
    "gsw-fr",
    "gu-in",
    "he-il",
    "hi-in",
    "hr-ba",
    "hr-hr",
    "hsb-de",
    "hu-hu",
    "hy-am",
    "id-id",
    "ig-ng",
    "ii-cn",
    "is-is",
    "it-ch",
    "it-it",
    "ja-jp",
    "ka-ge",
    "kk-kz",
    "kl-gl",
    "km-kh",
    "kn-in",
    "kok-in",
    "ko-kr",
    "ky-kg",
    "lb-lu",
    "lo-la",
    "lt-lt",
    "lv-lv",
    "mi-nz",
    "mk-mk",
    "ml-in",
    "mn-mn",
    "moh-ca",
    "mr-in",
    "ms-bn",
    "ms-my",
    "mt-mt",
    "nb-no",
    "ne-np",
    "nl-be",
    "nl-nl",
    "nn-no",
    "nso-za",
    "oc-fr",
    "or-in",
    "pa-in",
    "pl-pl",
    "prs-af",
    "ps-af",
    "pt-br",
    "pt-pt",
    "qut-gt",
    "quz-bo",
    "quz-ec",
    "quz-pe",
    "rm-ch",
    "ro-ro",
    "ru-ru",
    "rw-rw",
    "sah-ru",
    "sa-in",
    "se-fi",
    "se-no",
    "se-se",
    "si-lk",
    "sk-sk",
    "sl-si",
    "sma-no",
    "sma-se",
    "smj-no",
    "smj-se",
    "smn-fi",
    "sms-fi",
    "sq-al",
    "sv-fi",
    "sv-se",
    "sw-ke",
    "syr-sy",
    "ta-in",
    "te-in",
    "th-th",
    "tk-tm",
    "tn-za",
    "tr-tr",
    "tt-ru",
    "ug-cn",
    "uk-ua",
    "ur-pk",
    "vi-vn",
    "wo-sn",
    "xh-za",
    "yo-ng",
    "zh-cn",
    "zh-hk",
    "zh-mo",
    "zh-sg",
    "zh-tw",
    "zu-za",
    "az-cyrl-az",
    "az-latn-az",
    "bs-cyrl-ba",
    "bs-latn-ba",
    "ha-latn-ng",
    "iu-cans-ca",
    "iu-latn-ca",
    "mn-mong-cn",
    "sr-cyrl-ba",
    "sr-cyrl-cs",
    "sr-cyrl-me",
    "sr-cyrl-rs",
    "sr-latn-ba",
    "sr-latn-cs",
    "sr-latn-me",
    "sr-latn-rs",
    "tg-cyrl-tj",
    "tzm-latn-dz",
    "uz-cyrl-uz",
    "uz-latn-uz",
];

/// Words winget drops from a name or a publisher when they are not its first
/// word: `Famatech Corp.` is `famatech`.
const LEGAL_ENTITY_SUFFIXES: &[&str] = &[
    "ab",
    "ad",
    "ag",
    "aps",
    "as",
    "asa",
    "bv",
    "co",
    "cv",
    "doo",
    "ev",
    "ges",
    "gesmbh",
    "gmbh",
    "inc",
    "kg",
    "ks",
    "ps",
    "llc",
    "lp",
    "ltd",
    "ltda",
    "mbh",
    "nv",
    "plc",
    "pty",
    "pvt",
    "sa",
    "sarl",
    "sc",
    "sca",
    "sl",
    "sp",
    "spa",
    "srl",
    "sro",
    "company",
    "corp",
    "corporation",
    "holding",
    "holdings",
    "incorporated",
    "limited",
    "subsidiary",
];

/// What winget's normaliser makes of a program name: `Python 3.14.2 (64-bit)`
/// is `python`, `Mozilla Firefox (x64 en-US)` is `mozillafirefox`.
pub fn name(text: &str) -> String {
    let mut s = prepare(text);
    remove_architecture(&mut s);
    remove_locales(&mut s);
    s = P.kb.replace_all(&s, "$1").into_owned();
    while remove_all_program_patterns(&mut s) {}
    join(&s, |c| c.is_alphanumeric() || c == '+' || c == '&', false)
}

/// What winget's normaliser makes of a publisher: `Famatech Corp.` is
/// `famatech`, `Python Software Foundation` is `pythonsoftwarefoundation`.
pub fn publisher(text: &str) -> String {
    let mut s = prepare(text);
    while remove_all_publisher_patterns(&mut s) {}
    join(&s, char::is_alphanumeric, true)
}

/// The forms the catalogue may have stored a normalised name under: bare,
/// and with each architecture winget appends to it. Those two are the only
/// suffixes in the live catalogue's 19,509 rows.
pub fn architecture_suffixes(normalised: &str) -> [String; 3] {
    [
        normalised.to_string(),
        format!("{normalised}(X64)"),
        format!("{normalised}(X86)"),
    ]
}

/// Trim, cut at `@@` (searched from the fourth character, as winget does),
/// and take off any number of enclosing quotes or brackets.
fn prepare(text: &str) -> String {
    let mut s = text.trim().to_string();
    if let Some(at) = s.get(3..).and_then(|rest| rest.find("@@")) {
        s.truncate(3 + at);
    }
    loop {
        let unwrapped = if s.chars().count() >= 2
            && ((s.starts_with('"') && s.ends_with('"'))
                || (s.starts_with('(') && s.ends_with(')')))
        {
            s[1..s.len() - 1].to_string()
        } else {
            break;
        };
        s = unwrapped;
    }
    s
}

/// Remove every match of `re` that `accept` takes. `accept` sees the text and
/// the match's start and end, and answers where the removal ends, or `None`
/// to keep the match. A candidate that is refused is retried one character
/// on, which is how a look-behind behaves in the engine winget uses. Returns
/// whether anything changed.
fn remove_where(
    s: &mut String,
    re: &Regex,
    accept: impl Fn(&str, usize, usize) -> Option<usize>,
) -> bool {
    let mut out = String::with_capacity(s.len());
    let mut kept = 0;
    let mut at = 0;
    while at <= s.len() {
        let Some(m) = re.find_at(s, at) else { break };
        match accept(s, m.start(), m.end()).filter(|&end| end > m.start()) {
            Some(end) => {
                out.push_str(&s[kept..m.start()]);
                kept = end;
                at = end;
            }
            None => match after(s, m.start()) {
                Some(c) => at = m.start() + c.len_utf8(),
                None => break,
            },
        }
    }
    if kept == 0 {
        return false;
    }
    out.push_str(&s[kept..]);
    *s = out;
    true
}

fn before(s: &str, at: usize) -> Option<char> {
    s[..at].chars().next_back()
}

fn after(s: &str, at: usize) -> Option<char> {
    s[at..].chars().next()
}

fn always(_: &str, _: usize, end: usize) -> Option<usize> {
    Some(end)
}

/// `(?<=^|[^\p{L}\p{Nd}])`
fn word_start(s: &str, at: usize) -> bool {
    before(s, at).is_none_or(|c| !c.is_alphanumeric())
}

/// `(?<!\p{L})`
fn not_after_letter(s: &str, at: usize) -> bool {
    before(s, at).is_none_or(|c| !c.is_alphabetic())
}

/// `(?<!\p{L})` at the start of the match, nothing asked of its end.
fn after_no_letter(s: &str, start: usize, end: usize) -> Option<usize> {
    not_after_letter(s, start).then_some(end)
}

/// winget's architecture step, which removes the first kind it finds and
/// stops: `32/64-bit`, then x64 or 64-bit, then x86 or 32-bit, then ARM64,
/// then ARM32. A bare token followed by a digit (`x642`) is not an
/// architecture; `(?=\P{Nd}|$)` sits between the token and its optional
/// `Edition`, so it is checked on what follows the token itself.
fn remove_architecture(s: &mut String) {
    let p = &*P;
    let bits = |s: &mut String, re: &Regex| {
        remove_where(s, re, |s, start, end| word_start(s, start).then_some(end))
    };
    let token = |s: &mut String, re: &Regex| {
        remove_where(s, re, |s, start, end| {
            let token_end = match s[start..end].rfind(char::is_whitespace) {
                Some(space) if s[start + space..end].trim().eq_ignore_ascii_case("edition") => {
                    start + space
                }
                _ => end,
            };
            let no_digit = after(s, token_end).is_none_or(|c| !c.is_numeric());
            (word_start(s, start) && no_digit).then_some(end)
        })
    };
    let _ = bits(s, &p.arch_32_or_64)
        || token(s, &p.arch_x64)
        || bits(s, &p.arch_64_bit)
        || token(s, &p.arch_x32)
        || bits(s, &p.arch_32_bit)
        || token(s, &p.arch_arm64)
        || token(s, &p.arch_arm);
}

/// Remove the locale tags winget knows: `(?<![A-Z])` before the tag,
/// `(?![A-Z])` after its region, an optional `-valencia`, and the tag must be
/// in [`LOCALES`]. A tag-shaped word that is not a locale stays.
fn remove_locales(s: &mut String) {
    remove_where(s, &P.locale, |s, start, end| {
        if !not_after_letter(s, start) || after(s, end).is_some_and(char::is_alphabetic) {
            return None;
        }
        let tag = s[start..end].to_lowercase();
        let valencia = "-valencia";
        let with_valencia = s
            .get(end..end + valencia.len())
            .filter(|v| v.eq_ignore_ascii_case(valencia))
            .map(|_| (format!("{tag}{valencia}"), end + valencia.len()));
        match with_valencia {
            Some((long, long_end)) if LOCALES.contains(&long.as_str()) => Some(long_end),
            _ => LOCALES.contains(&tag.as_str()).then_some(end),
        }
    });
}

/// `VersionDelimited`: a number with at least one separator, optionally
/// introduced by a version word. The word is matched backwards from the
/// number, since it sits inside an optional group with its own look-behind.
fn remove_version_delimited(s: &mut String) -> bool {
    let mut out = String::with_capacity(s.len());
    let mut kept = 0;
    let mut at = 0;
    let mut changed = false;
    while let Some(m) = P.version_delimited.find_at(s, at) {
        let mut start = m.start();
        if let Some(w) = P.version_word_before.find(&s[kept..m.start()]) {
            let word_start = kept + w.start();
            if not_after_letter(s, word_start) {
                start = word_start;
            }
        }
        out.push_str(&s[kept..start]);
        kept = m.end();
        at = m.end();
        changed = true;
    }
    if changed {
        out.push_str(&s[kept..]);
        *s = out;
    }
    changed
}

/// `Version`: a version word and a number, `v2`, `Build 1234`, `for P6`.
fn remove_version(s: &mut String) -> bool {
    let mut out = String::with_capacity(s.len());
    let mut kept = 0;
    let mut at = 0;
    let mut changed = false;
    while at <= s.len() {
        let Some(c) = P.version.captures_at(s, at) else {
            break;
        };
        let m = c.get(0).expect("a match has a whole");
        // The look-behind sits after the optional `FOR\s`, so with it present
        // it is satisfied by the space.
        let word_at = c.get(1).map_or(m.start(), |f| f.end());
        if not_after_letter(s, word_at) {
            out.push_str(&s[kept..m.start()]);
            kept = m.end();
            at = m.end();
            changed = true;
        } else {
            at = m.start() + s[m.start()..].chars().next().map_or(1, char::len_utf8);
        }
    }
    if changed {
        out.push_str(&s[kept..]);
        *s = out;
    }
    changed
}

fn remove_plain(s: &mut String, re: &Regex) -> bool {
    remove_where(s, re, always)
}

/// One pass of winget's `ProgramNameRegexes`, in its order.
fn remove_all_program_patterns(s: &mut String) -> bool {
    let p = &*P;
    let mut changed = false;
    changed |= remove_plain(s, &p.prefix_parens);
    changed |= remove_plain(s, &p.empty_parens);
    changed |= remove_plain(s, &p.file_path_ghs);
    changed |= remove_plain(s, &p.file_path_parens);
    changed |= remove_plain(s, &p.file_path_quotes);
    changed |= remove_plain(s, &p.file_path);
    changed |= remove_where(s, &p.version_letter, after_no_letter);
    changed |= remove_version_delimited(s);
    changed |= remove_version(s);
    changed |= remove_plain(s, &p.en);
    changed |= remove_plain(s, &p.non_nested_bracket);
    changed |= remove_plain(s, &p.bracket_enclosed);
    changed |= remove_where(s, &p.uri_protocol, after_no_letter);
    changed |= remove_plain(s, &p.leading_symbols);
    changed |= remove_plain(s, &p.trailing_symbols);
    changed
}

/// One pass of winget's `PublisherNameRegexes`, in its order.
fn remove_all_publisher_patterns(s: &mut String) -> bool {
    let p = &*P;
    let mut changed = false;
    changed |= remove_version_delimited(s);
    changed |= remove_version(s);
    changed |= remove_plain(s, &p.non_nested_bracket);
    changed |= remove_plain(s, &p.bracket_enclosed);
    changed |= remove_where(s, &p.uri_protocol, after_no_letter);
    changed |= remove_non_letter_words(s);
    changed |= remove_plain(s, &p.trailing_non_letters);
    changed |= remove_acronym_separators(s);
    changed
}

/// `NonLetters`: a whitespace-separated word with no letter in it, such as
/// the `2000` in `Foo 2000 Ltd`.
fn remove_non_letter_words(s: &mut String) -> bool {
    let words: Vec<&str> = s.split_whitespace().collect();
    let kept: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| w.chars().any(char::is_alphabetic))
        .collect();
    if kept.len() == words.len() {
        return false;
    }
    *s = kept.join(" ");
    true
}

/// `AcronymSeparators`: the dots in `S.A.` and the slash in `A/S`, between
/// single letters, so that the suffix reads as one word and is dropped.
fn remove_acronym_separators(s: &mut String) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut changed = false;
    for (i, &c) in chars.iter().enumerate() {
        let separator = matches!(c, '.' | '/')
            && i >= 1
            && chars[i - 1].is_alphabetic()
            && (i < 2 || !chars[i - 2].is_alphabetic())
            && chars.get(i + 1).is_some_and(|n| n.is_alphabetic())
            && chars.get(i + 2).is_none_or(|n| !n.is_alphabetic());
        if separator {
            changed = true;
        } else {
            out.push(c);
        }
    }
    if changed {
        *s = out;
    }
    changed
}

/// Split into words, drop the legal-entity suffixes that are not the first
/// word (a publisher stops at the first one, a name skips it and carries
/// on), and keep only letters and digits, folded.
fn join(s: &str, keeps: impl Fn(char) -> bool, stop_at_suffix: bool) -> String {
    let mut out = String::new();
    let mut first = true;
    for word in s.split(|c: char| !keeps(c)).filter(|w| !w.is_empty()) {
        if !first && LEGAL_ENTITY_SUFFIXES.contains(&word.to_lowercase().as_str()) {
            if stop_at_suffix {
                break;
            }
            continue;
        }
        first = false;
        out.extend(
            word.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two the catalogue was seen to store, from the spec's Open list.
    #[test]
    fn the_examples_the_catalogue_stores() {
        assert_eq!(name("Python 3.0"), "python");
        assert_eq!(name("Mozilla Firefox (en-US)"), "mozillafirefox");
    }

    /// Display names from the development machine's registry, each beside
    /// the value the live catalogue holds for the same package.
    #[test]
    fn real_display_names_fold_to_what_the_catalogue_holds() {
        for (display, stored) in [
            ("Advanced IP Scanner 2.5.1", "advancedipscanner"),
            (
                "AOMEI Partition Assistant 10.9.0",
                "aomeipartitionassistant",
            ),
            ("CPUID HWMonitor 1.56", "cpuidhwmonitor"),
            ("Python 3.14.2 (64-bit)", "python"),
            ("7-Zip 26.03 (x64)", "7zip"),
            ("Mozilla Firefox (x64 en-US)", "mozillafirefox"),
            ("Notepad++ (64-bit x64)", "notepad++"),
            (
                "Microsoft Visual Studio Code (User)",
                "microsoftvisualstudiocode",
            ),
            ("Beeper 4.2.957", "beeper"),
            ("BridgeLink v0.1.0", "bridgelink"),
        ] {
            assert_eq!(name(display), stored.replace('+', ""), "{display}");
        }
    }

    /// What it must leave alone. A number with no separator is part of the
    /// name (`RootsMagic 6`, `Hotspot 25`), a locale-shaped word that is not
    /// a locale stays, and an architecture glued to a digit is not one.
    #[test]
    fn what_is_part_of_the_name_stays() {
        assert_eq!(name("RootsMagic 6"), "rootsmagic6");
        assert_eq!(
            name("Eclipse Temurin JDK with Hotspot 25"),
            "eclipsetemurinjdkwithhotspot25"
        );
        assert_eq!(name("Revo Scan 5 MetroX"), "revoscan5metrox");
        assert_eq!(name("Cities: Skylines II"), "citiesskylinesii");
        assert_eq!(name("Grand Prix zz-QQ Edition"), "grandprixzzqqedition");
        assert_eq!(name("x642 Tool"), "x642tool");
    }

    /// A legal-entity word after the first is dropped from a name, and a
    /// publisher stops at it.
    #[test]
    fn legal_entity_suffixes() {
        assert_eq!(publisher("Famatech Corp."), "famatech");
        assert_eq!(
            publisher("Python Software Foundation"),
            "pythonsoftwarefoundation"
        );
        assert_eq!(publisher("Igor Pavlov"), "igorpavlov");
        assert_eq!(publisher("Mozilla Corporation"), "mozilla");
        assert_eq!(publisher("Valve Corp."), "valve");
        assert_eq!(publisher("Opera Norway AS"), "operanorway");
        assert_eq!(publisher("Foo S.A."), "foo");
        // The first word is never a suffix, whatever it spells.
        assert_eq!(publisher("AB Software"), "absoftware");
    }

    /// Brackets of every kind go, and so does a protocol.
    #[test]
    fn brackets_and_addresses() {
        assert_eq!(name("Microsoft Edge [beta]"), "microsoftedge");
        assert_eq!(name("(Prefix) Tool"), "tool");
        assert_eq!(name("Tool (KB123456)"), "toolkb123456");
        assert_eq!(publisher("https://example.org"), "exampleorg");
    }

    #[test]
    fn the_architecture_forms_the_catalogue_writes() {
        assert_eq!(
            architecture_suffixes("python"),
            [
                "python".to_string(),
                "python(X64)".to_string(),
                "python(X86)".to_string()
            ]
        );
    }
}
