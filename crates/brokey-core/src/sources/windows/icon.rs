//! Turning a `DisplayIcon` registry value into a picture the page can draw.
//!
//! `DisplayIcon` names a file and, optionally, which icon inside it: a path
//! on its own, a path and a resource index after a comma, or (rarely) an
//! `.ico` file, which carries its own icons and needs no index. [`reference`]
//! is the pure parser for that value. The file it names is usually a binary
//! rather than a picture, and the asset protocol's scope does not admit
//! Program Files in any case, so [`cached`] extracts the icon into Brokey's
//! own cache, under `$CACHE/**`, which the scope does admit, and hands back
//! the path to the `.ico` it wrote there.
//!
//! Nothing here calls a Windows API. Extraction is [`pe::icon`], from the
//! previous task, which is itself a pure function of bytes; the only impure
//! parts of this module are reading the source file and writing the cache,
//! both ordinary filesystem calls that run the same on every platform. That
//! is why its tests write their own files into a temporary directory and
//! never touch the registry.

use crate::sources::windows::pe::{self, Wanted};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// What a `DisplayIcon` value points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub path: PathBuf,
    pub wanted: Wanted,
}

/// The four bytes an `.ico` file always begins with: a reserved word of
/// zero, then a type of 1 (icon, as opposed to 2, cursor).
const ICO_MAGIC: [u8; 4] = [0x00, 0x00, 0x01, 0x00];

/// Split a `DisplayIcon` value into a file and which icon inside it.
///
/// The last comma is the split point, and only then when everything after it
/// trims to an integer: a comma inside a directory name, such as
/// `Acme, Inc`, is not an index and is left in the path. One matching pair of
/// surrounding double quotes is stripped before trimming, because the index,
/// when there is one, sits outside the quotes. A negative index names a
/// resource id rather than a position; one too large for a `u16` cannot be an
/// id, so it falls back to the first icon rather than to an id that could
/// never match.
pub fn reference(display_icon: &str) -> Option<Reference> {
    let raw = display_icon.trim();
    let (path, wanted) = match raw.rsplit_once(',') {
        Some((path, index)) => match index.trim().parse::<i64>() {
            Ok(n) if n < 0 => {
                // `unsigned_abs` rather than negating `n`: `n` can be
                // `i64::MIN`, and `-n` on that value overflows and panics
                // with overflow checks on, which is every debug build. A
                // `DisplayIcon` is machine data and this is the one place a
                // hostile value must not reach a panic.
                let wanted = u16::try_from(n.unsigned_abs())
                    .map(Wanted::Id)
                    .unwrap_or(Wanted::Nth(0));
                (path, wanted)
            }
            Ok(n) => (path, Wanted::Nth(n as usize)),
            Err(_) => (raw, Wanted::Nth(0)),
        },
        None => (raw, Wanted::Nth(0)),
    };
    let path = path.trim();
    let path = path
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(path)
        .trim();
    if path.is_empty() {
        return None;
    }
    Some(Reference {
        path: PathBuf::from(path),
        wanted,
    })
}

/// The largest file [`cached`] will read an icon out of.
///
/// `reference.path` comes from an Add or remove programs `DisplayIcon`
/// value, which is a string whatever installer wrote that key put there.
/// Nothing in the registry bounds it, so without a limit a value naming a
/// very large file makes Brokey allocate that file whole, once per entry,
/// while the Installed page is listing applications. A pagefile, a virtual
/// machine disk or an ISO named there would each be read in full.
///
/// The figure is 256 MiB, and it is not the "a few megabytes is generous
/// for an executable" limit it ought to be, because that instinct is wrong
/// and was measured to be wrong. Over the 132 `DisplayIcon` values in this
/// machine's three uninstall keys, 131 of which name a file that is
/// actually there: the median is 1.4 MB, but 53 are over 4 MB, 24 over
/// 8 MB and 17 over 32 MB. An Electron application's icon carrier is the
/// application, so `Code - Insiders.exe` at 237 MB and `Vortex.exe` at
/// 211 MB are ordinary entries, and each carries the icon the page draws
/// for it. A few megabytes would have taken the icon off two fifths of the
/// applications on this machine.
///
/// So the limit sits above the body of that distribution rather than above
/// an intuition. One file here exceeds it, `Root.exe` at 616 MB, and loses
/// its icon and draws the fallback: that is the trade this takes
/// deliberately, one entry in 131 against a read with no bound at all.
const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;

/// A file's bytes, or `None` when it is missing, unreadable, or larger than
/// `limit`.
///
/// The handle is opened first and its own metadata asked, rather than
/// `std::fs::metadata` on the path, and the read is bounded by `take` as
/// well: a file that grows between the question and the read is then still
/// bounded, instead of the size check being advice the read need not
/// follow. `limit` is an argument rather than [`MAX_SOURCE_BYTES`] read
/// directly so that a test can put both sides of the rule without writing
/// a quarter of a gigabyte to disk.
fn read_within(path: &Path, limit: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    if size > limit {
        return None;
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(limit).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// A stable file name for a `(path, wanted)` pair, so the same reference
/// always names the same cache entry and two different ones are
/// overwhelmingly unlikely to collide (a 64-bit hash can collide; nothing
/// here guards against it, because a cache entry is a performance detail, not
/// a security boundary). This names a cache entry and guards nothing, so
/// `DefaultHasher` is enough; what it does need is to be stable across runs
/// of the same binary, because the cache is on disk and is read by the next
/// launch, and `DefaultHasher::new()` gives exactly that.
fn cache_name(reference: &Reference) -> String {
    let mut hasher = DefaultHasher::new();
    reference
        .path
        .to_string_lossy()
        .to_lowercase()
        .hash(&mut hasher);
    match reference.wanted {
        Wanted::Nth(n) => {
            0u8.hash(&mut hasher);
            n.hash(&mut hasher);
        }
        Wanted::Id(id) => {
            1u8.hash(&mut hasher);
            id.hash(&mut hasher);
        }
    }
    format!("{:016x}.ico", hasher.finish())
}

/// The cached `.ico` for a reference, extracting it the first time. `None`
/// when the file is gone, unreadable, larger than [`MAX_SOURCE_BYTES`], or
/// carries no icon.
///
/// The cache is named from a hash of the lowercased path together with the
/// index, under `cache.join("icons")`. When that file is already there it is
/// returned without touching the source at all. Otherwise the source is
/// read, within the limit above: four leading bytes of `00 00 01 00` mean it
/// is already an `.ico` and
/// it is copied through untouched; anything else goes to [`pe::icon`]. The
/// result is written to a temporary name unique to this call, in the same
/// directory as `dest`, and renamed into place, so a single caller never
/// serves a file it has only partly written. Two callers can still race to
/// extract the same icon at once: each writes its own temporary, but only one
/// rename can land on `dest` first. The loser's rename then fails because its
/// source has moved away under it, and that is treated as success, since the
/// winner's file is already at `dest`, rather than as a missing icon.
///
/// A cache entry never expires. Once written it is served for as long as it
/// exists, even after the application it came from is updated in place or
/// removed entirely; nothing here checks a modification time or prunes an
/// entry. The brief asked for extraction, not invalidation, and the
/// `cached` test that extracts once and reads the cache the second time
/// relies on exactly this.
pub fn cached(cache: &Path, reference: &Reference) -> Option<PathBuf> {
    let dir = cache.join("icons");
    let dest = dir.join(cache_name(reference));
    if dest.is_file() {
        return Some(dest);
    }

    let bytes = read_within(&reference.path, MAX_SOURCE_BYTES)?;
    let ico = if bytes.get(0..4) == Some(&ICO_MAGIC[..]) {
        bytes
    } else {
        pe::icon(&bytes, reference.wanted)?
    };

    std::fs::create_dir_all(&dir).ok()?;
    // Unique per call, not just per reference: two threads extracting the
    // same icon at once must not share a temporary file, or one's write can
    // truncate the other's before either renames. The process id alone is
    // not enough, because two threads of the same process share it.
    let tmp = dir.join(format!(
        "{}.{}.{}.tmp",
        cache_name(reference),
        std::process::id(),
        unique_counter()
    ));
    std::fs::write(&tmp, &ico).ok()?;
    if std::fs::rename(&tmp, &dest).is_err() {
        // Another caller's rename could have won the race in between: that
        // is success, not failure, so check for its result before giving up.
        let _ = std::fs::remove_file(&tmp);
        if !dest.is_file() {
            return None;
        }
    }
    Some(dest)
}

/// A number unique to each call within this process, for [`cached`]'s
/// temporary file name.
fn unique_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::windows::pe::{RT_GROUP_ICON, RT_ICON};

    /// A minimal PE carrying one or more icon groups, built rather than
    /// checked in so the exact bytes that produce each answer are visible
    /// here. This mirrors the builder `pe`'s own tests use: `pe::icon` is
    /// already proven correct there, so this one only has to produce
    /// something valid enough to exercise `cached` around it, not to
    /// re-prove the PE reader itself.
    struct Builder {
        resources: Vec<(u32, u16, Vec<u8>)>,
    }

    impl Builder {
        fn new() -> Builder {
            Builder {
                resources: Vec::new(),
            }
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

        /// Lay the whole thing out as a 64-bit PE with one section holding
        /// the resource directory. The section's virtual address is
        /// deliberately not its file offset, matching what `pe`'s own tests
        /// check.
        fn build(self) -> Vec<u8> {
            use std::collections::HashMap;

            fn put_u16(buf: &mut [u8], at: usize, v: u16) {
                buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
            }
            fn put_u32(buf: &mut [u8], at: usize, v: u32) {
                buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }

            let mut type_order: Vec<u32> = Vec::new();
            let mut by_type: HashMap<u32, Vec<(u16, Vec<u8>)>> = HashMap::new();
            for (type_id, id, bytes) in self.resources {
                if !type_order.contains(&type_id) {
                    type_order.push(type_id);
                }
                by_type.entry(type_id).or_default().push((id, bytes));
            }
            let id_lists: Vec<&Vec<(u16, Vec<u8>)>> = type_order
                .iter()
                .map(|t| by_type.get(t).expect("grouped above"))
                .collect();

            let type_dir_size = 16 + 8 * type_order.len();
            let id_dir_sizes: Vec<usize> = id_lists.iter().map(|ids| 16 + 8 * ids.len()).collect();
            let total_ids: usize = id_lists.iter().map(|ids| ids.len()).sum();
            let lang_dir_size = 16 + 8;
            let data_entry_size = 16;

            let type_dir_offset = 0usize;
            let mut id_dir_offset = Vec::with_capacity(type_order.len());
            {
                let mut cursor = type_dir_offset + type_dir_size;
                for size in &id_dir_sizes {
                    id_dir_offset.push(cursor);
                    cursor += size;
                }
            }
            let lang_dir_base =
                type_dir_offset + type_dir_size + id_dir_sizes.iter().sum::<usize>();
            let data_entry_base = lang_dir_base + total_ids * lang_dir_size;
            let payload_base = data_entry_base + total_ids * data_entry_size;

            let mut lang_dir_offset = Vec::with_capacity(total_ids);
            let mut data_entry_offset = Vec::with_capacity(total_ids);
            let mut payload_offset = Vec::with_capacity(total_ids);
            let mut lang_cursor = lang_dir_base;
            let mut data_cursor = data_entry_base;
            let mut payload_cursor = payload_base;
            for ids in &id_lists {
                for (_, bytes) in ids.iter() {
                    lang_dir_offset.push(lang_cursor);
                    lang_cursor += lang_dir_size;
                    data_entry_offset.push(data_cursor);
                    data_cursor += data_entry_size;
                    payload_offset.push(payload_cursor);
                    payload_cursor += bytes.len();
                }
            }
            let content_size = payload_cursor;

            let pe_offset = 0x40usize;
            let optional_header_size = 0x70 + 24;
            let section_table_size = 40;
            let section_file_offset =
                pe_offset + 4 + 20 + optional_header_size + section_table_size;
            let section_va = section_file_offset + 0x4000;

            let mut content = vec![0u8; content_size];

            put_u16(&mut content, type_dir_offset + 12, 0);
            put_u16(&mut content, type_dir_offset + 14, type_order.len() as u16);
            for (i, type_id) in type_order.iter().enumerate() {
                let entry = type_dir_offset + 16 + i * 8;
                put_u32(&mut content, entry, *type_id);
                put_u32(
                    &mut content,
                    entry + 4,
                    0x8000_0000u32 | (id_dir_offset[i] as u32),
                );
            }

            {
                let mut flat = 0usize;
                for (i, ids) in id_lists.iter().enumerate() {
                    let base = id_dir_offset[i];
                    put_u16(&mut content, base + 12, 0);
                    put_u16(&mut content, base + 14, ids.len() as u16);
                    for (j, (id, _bytes)) in ids.iter().enumerate() {
                        let entry = base + 16 + j * 8;
                        put_u32(&mut content, entry, *id as u32);
                        put_u32(
                            &mut content,
                            entry + 4,
                            0x8000_0000u32 | (lang_dir_offset[flat] as u32),
                        );
                        flat += 1;
                    }
                }
            }

            {
                let mut flat = 0usize;
                for ids in &id_lists {
                    for (_, bytes) in ids.iter() {
                        let lang_base = lang_dir_offset[flat];
                        put_u16(&mut content, lang_base + 12, 0);
                        put_u16(&mut content, lang_base + 14, 1);
                        put_u32(&mut content, lang_base + 16, 0);
                        put_u32(&mut content, lang_base + 20, data_entry_offset[flat] as u32);

                        let data_base = data_entry_offset[flat];
                        let rva = section_va + payload_offset[flat];
                        put_u32(&mut content, data_base, rva as u32);
                        put_u32(&mut content, data_base + 4, bytes.len() as u32);

                        let p = payload_offset[flat];
                        content[p..p + bytes.len()].copy_from_slice(bytes);

                        flat += 1;
                    }
                }
            }

            let mut file = Vec::new();
            file.extend_from_slice(b"MZ");
            file.resize(0x3C, 0);
            file.extend_from_slice(&(pe_offset as u32).to_le_bytes());

            file.extend_from_slice(b"PE\0\0");
            file.extend_from_slice(&0x8664u16.to_le_bytes()); // machine, unused by the reader
            file.extend_from_slice(&1u16.to_le_bytes()); // number of sections
            file.extend_from_slice(&0u32.to_le_bytes()); // timestamp
            file.extend_from_slice(&0u32.to_le_bytes()); // pointer to symbol table
            file.extend_from_slice(&0u32.to_le_bytes()); // number of symbols
            file.extend_from_slice(&(optional_header_size as u16).to_le_bytes());
            file.extend_from_slice(&0u16.to_le_bytes()); // characteristics

            let optional_start = file.len();
            file.extend_from_slice(&0x20bu16.to_le_bytes()); // PE32+ magic
            file.resize(optional_start + 0x70, 0);
            file.extend_from_slice(&0u32.to_le_bytes()); // data directory 0: RVA
            file.extend_from_slice(&0u32.to_le_bytes()); // data directory 0: size
            file.extend_from_slice(&0u32.to_le_bytes()); // data directory 1: RVA
            file.extend_from_slice(&0u32.to_le_bytes()); // data directory 1: size
            file.extend_from_slice(&(section_va as u32).to_le_bytes()); // resource table RVA
            file.extend_from_slice(&(content_size as u32).to_le_bytes()); // resource table size

            let mut name_field = [0u8; 8];
            name_field[..5].copy_from_slice(b".rsrc");
            file.extend_from_slice(&name_field);
            file.extend_from_slice(&(content_size as u32).to_le_bytes()); // virtual size
            file.extend_from_slice(&(section_va as u32).to_le_bytes()); // virtual address
            file.extend_from_slice(&(content_size as u32).to_le_bytes()); // size of raw data
            file.extend_from_slice(&(section_file_offset as u32).to_le_bytes()); // pointer to raw data
            file.extend_from_slice(&0u32.to_le_bytes()); // pointer to relocations
            file.extend_from_slice(&0u32.to_le_bytes()); // pointer to line numbers
            file.extend_from_slice(&0u16.to_le_bytes()); // number of relocations
            file.extend_from_slice(&0u16.to_le_bytes()); // number of line numbers
            file.extend_from_slice(&0u32.to_le_bytes()); // characteristics

            file.extend_from_slice(&content);
            file
        }
    }

    /// One icon group carrying one small image.
    fn pe_with_one_icon() -> Vec<u8> {
        Builder::new()
            .add(RT_ICON, 1, vec![0xAA; 32])
            .group(1, &[(16, 1)])
            .build()
    }

    /// Two icon groups, each carrying one small image, so `Wanted::Nth(0)`
    /// and `Wanted::Nth(1)` name two different, genuinely distinct icons.
    fn pe_with_two_icons() -> Vec<u8> {
        Builder::new()
            .add(RT_ICON, 1, vec![0xAA; 32])
            .add(RT_ICON, 2, vec![0xBB; 48])
            .group(1, &[(16, 1)])
            .group(2, &[(24, 2)])
            .build()
    }

    /// A temporary directory for one test, removed when the returned guard
    /// drops. `http.rs` already uses `tempfile::tempdir()`, and `tempfile` is
    /// already a dev-dependency of this crate, so there is no reason to roll
    /// a private one here.
    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("a temporary directory")
    }

    #[test]
    fn a_display_icon_splits_into_a_file_and_which_icon_in_it() {
        let cases = [
            // A bare path, so the first icon.
            (
                "C:\\Program Files\\Cheat Engine\\Cheat Engine.exe",
                "C:\\Program Files\\Cheat Engine\\Cheat Engine.exe",
                Wanted::Nth(0),
            ),
            // The common form: a path and a zero.
            (
                "C:\\Program Files\\Vortex\\Vortex.exe,0",
                "C:\\Program Files\\Vortex\\Vortex.exe",
                Wanted::Nth(0),
            ),
            // Quoted, with the index outside the quotes.
            (
                "\"C:\\Program Files\\Microsoft OneDrive\\OneDrive.App.exe\",1",
                "C:\\Program Files\\Microsoft OneDrive\\OneDrive.App.exe",
                Wanted::Nth(1),
            ),
            // A negative index is a resource id, not a position.
            (
                "C:\\Program Files\\Microsoft OneDrive\\OneDriveSetup.exe,-101",
                "C:\\Program Files\\Microsoft OneDrive\\OneDriveSetup.exe",
                Wanted::Id(101),
            ),
            // An 8.3 path, which Windows opens as it is.
            (
                "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE,0",
                "C:\\PROGRA~1\\DIFX\\873032~1\\DPINST~1.EXE",
                Wanted::Nth(0),
            ),
            // An .ico file, which carries its own icons and needs no index.
            (
                "C:\\Program Files\\Git\\mingw64\\share\\git\\git-for-windows.ico",
                "C:\\Program Files\\Git\\mingw64\\share\\git\\git-for-windows.ico",
                Wanted::Nth(0),
            ),
            // A comma inside a directory name is not an index.
            (
                "C:\\Program Files\\Acme, Inc\\app.exe",
                "C:\\Program Files\\Acme, Inc\\app.exe",
                Wanted::Nth(0),
            ),
            // Surrounding space is trimmed.
            ("  C:\\a\\b.exe , 3 ", "C:\\a\\b.exe", Wanted::Nth(3)),
            // A negative index too large for a u16 cannot be a resource id,
            // so it falls back to the first icon. i64::MIN is the case that
            // matters: negating it overflows, so this is also the
            // regression test for the panic that fix guards against.
            (
                "C:\\a\\b.exe,-9223372036854775808",
                "C:\\a\\b.exe",
                Wanted::Nth(0),
            ),
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

    #[test]
    fn an_ico_on_disk_is_copied_into_the_cache_rather_than_parsed() {
        let dir = tempdir();
        let source = dir.path().join("app.ico");
        let mut bytes = ICO_MAGIC.to_vec();
        bytes.extend_from_slice(b"not really an icon directory, just bytes to copy");
        std::fs::write(&source, &bytes).unwrap();

        let cache = dir.path().join("cache");
        let r = Reference {
            path: source.clone(),
            wanted: Wanted::Nth(0),
        };
        let out = cached(&cache, &r).expect("an .ico file is always usable");
        assert!(
            out.starts_with(&cache),
            "{} should be under {}",
            out.display(),
            cache.display()
        );
        assert_eq!(std::fs::read(&out).unwrap(), bytes, "copied byte for byte");
    }

    #[test]
    fn a_binary_is_extracted_once_and_read_from_the_cache_after() {
        let dir = tempdir();
        let source = dir.path().join("app.exe");
        std::fs::write(&source, pe_with_one_icon()).unwrap();

        let cache = dir.path().join("cache");
        let r = Reference {
            path: source.clone(),
            wanted: Wanted::Nth(0),
        };
        let first = cached(&cache, &r).expect("the pe carries an icon");

        std::fs::remove_file(&source).unwrap();

        let second = cached(&cache, &r).expect("the cache still has it");
        assert_eq!(first, second, "the second call answers with the same path");
    }

    #[test]
    fn a_file_that_is_not_there_gives_no_picture() {
        let dir = tempdir();
        let cache = dir.path().join("cache");
        let r = Reference {
            path: dir.path().join("nothing-here.exe"),
            wanted: Wanted::Nth(0),
        };
        assert_eq!(cached(&cache, &r), None);
    }

    /// Both sides of the size limit, put to a file of a known length with
    /// the limit as an argument, so the rule is tested without writing a
    /// quarter of a gigabyte to disk. A file exactly at the limit is read,
    /// because the limit is what is allowed and not what is refused, and
    /// one byte more is not read at all.
    #[test]
    fn a_file_over_the_limit_is_never_read() {
        let dir = tempdir();
        let source = dir.path().join("carrier.bin");
        std::fs::write(&source, vec![0x5Au8; 64]).unwrap();

        assert_eq!(
            read_within(&source, 64).as_deref().map(<[u8]>::len),
            Some(64),
            "a file exactly at the limit is read"
        );
        assert_eq!(
            read_within(&source, 65).as_deref().map(<[u8]>::len),
            Some(64),
            "a file under the limit is read"
        );
        assert_eq!(
            read_within(&source, 63),
            None,
            "one byte over the limit is not read"
        );
        assert_eq!(
            read_within(&source, 0),
            None,
            "a limit of nothing reads nothing"
        );

        let missing = dir.path().join("nothing-here.bin");
        assert_eq!(read_within(&missing, 1024), None, "a missing file is None");
    }

    /// The limit `cached` actually uses, pinned so that changing it is a
    /// deliberate act with this file's doc comment in front of the person
    /// changing it. 256 MiB was chosen from the sizes of the 131 resolvable
    /// `DisplayIcon` files on the development machine, not from an
    /// intuition about how big an executable is.
    #[test]
    fn the_limit_is_the_measured_one() {
        assert_eq!(MAX_SOURCE_BYTES, 268_435_456);
    }

    #[test]
    fn a_file_with_no_icon_in_it_gives_no_picture() {
        let dir = tempdir();
        let source = dir.path().join("random.bin");
        std::fs::write(&source, [0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]).unwrap();

        let cache = dir.path().join("cache");
        let r = Reference {
            path: source,
            wanted: Wanted::Nth(0),
        };
        assert_eq!(cached(&cache, &r), None);
    }

    #[test]
    fn the_cache_name_is_the_path_and_the_index_together() {
        let dir = tempdir();
        let source = dir.path().join("app.exe");
        std::fs::write(&source, pe_with_two_icons()).unwrap();

        let cache = dir.path().join("cache");
        let first = cached(
            &cache,
            &Reference {
                path: source.clone(),
                wanted: Wanted::Nth(0),
            },
        )
        .expect("the first icon");
        let second = cached(
            &cache,
            &Reference {
                path: source.clone(),
                wanted: Wanted::Nth(1),
            },
        )
        .expect("the second icon");
        assert_ne!(first, second, "two indices give two files");

        let first_again = cached(
            &cache,
            &Reference {
                path: source,
                wanted: Wanted::Nth(0),
            },
        )
        .expect("the same reference again");
        assert_eq!(first, first_again, "the same path and index give one file");
    }
}
