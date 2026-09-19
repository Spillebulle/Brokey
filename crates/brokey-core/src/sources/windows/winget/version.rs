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
        while i < bytes.len()
            && bytes[i].is_ascii_alphanumeric()
            && bytes[i].is_ascii_digit() == digit
        {
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

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::{cmp, newer};

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
