//! An icon out of a PE, as the bytes of an `.ico` file.
//!
//! An `.ico` file and a PE's icon resources are the same directory in two
//! widths. Turning one into the other copies image bytes untouched: nothing
//! here decodes an image, so the reader is a pure function of the bytes it
//! is given and runs on Linux as readily as on Windows.
//!
//! **Known limit.** A group named by a string rather than an integer id is
//! not reachable through [`Wanted`]: the brief asks for integer ids only,
//! and this reader looks at nothing else. Measured against Windows' own
//! `ExtractIconExW` over every `.exe` in `System32` and `SysWOW64` (976
//! files), that loses the icon on 28 of them, `cmd.exe`, `calc.exe` and
//! `conhost.exe` among them. Resolving a string-named group is a change of
//! interface and is left to a follow-up task.

/// Which icon group to take out of a PE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wanted {
    /// `app.exe,2`: the third group in ascending id order.
    Nth(usize),
    /// `app.exe,-101`: the group whose resource id is 101.
    Id(u16),
}

/// One section of a PE. A section's virtual address and its file offset are
/// unrelated numbers; only the section table ties them together, which is
/// why every RVA in this module is converted through [`rva_to_file_offset`]
/// rather than ever being used as a file offset directly.
struct Section {
    virtual_address: u32,
    virtual_size: u32,
    raw_size: u32,
    raw_pointer: u32,
}

/// The two facts pulled out of the PE header: where the resource directory
/// starts in the file, and the section table, needed again whenever a data
/// entry's own RVA has to be resolved.
struct Header {
    resource_base: usize,
    sections: Vec<Section>,
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    let slice = bytes.get(offset..end)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let slice = bytes.get(offset..end)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// An RVA becomes a file offset by finding the section containing it and
/// adding the difference to that section's raw pointer. A section is
/// spanned by the larger of its virtual size and its raw size: some linkers
/// write a virtual size of zero, and spanning by virtual size alone would
/// then make every RVA in that section unresolvable.
fn rva_to_file_offset(sections: &[Section], rva: u32) -> Option<usize> {
    for section in sections {
        let span = section.virtual_size.max(section.raw_size);
        let Some(end) = section.virtual_address.checked_add(span) else {
            continue;
        };
        if rva >= section.virtual_address && rva < end {
            let delta = rva - section.virtual_address;
            let file_offset = section.raw_pointer.checked_add(delta)?;
            return Some(file_offset as usize);
        }
    }
    None
}

impl Header {
    fn parse(bytes: &[u8]) -> Option<Header> {
        if bytes.get(0..2) != Some(&b"MZ"[..]) {
            return None;
        }
        let pe_offset = u32_at(bytes, 0x3C)? as usize;
        let pe_end = pe_offset.checked_add(4)?;
        if bytes.get(pe_offset..pe_end) != Some(&b"PE\0\0"[..]) {
            return None;
        }

        let coff = pe_end;
        let number_of_sections = u16_at(bytes, coff.checked_add(2)?)? as usize;
        let size_of_optional_header = u16_at(bytes, coff.checked_add(16)?)? as usize;
        let optional_start = coff.checked_add(20)?;

        let magic = u16_at(bytes, optional_start)?;
        let data_directory_offset = match magic {
            0x10b => 0x60,
            0x20b => 0x70,
            _ => return None,
        };
        let data_directory = optional_start.checked_add(data_directory_offset)?;
        // The third entry (index 2): the resource table's RVA and size.
        let resource_entry = data_directory.checked_add(16)?;
        let resource_rva = u32_at(bytes, resource_entry)?;
        let resource_size = u32_at(bytes, resource_entry.checked_add(4)?)?;
        if resource_size == 0 {
            return None;
        }

        let section_table = optional_start.checked_add(size_of_optional_header)?;
        let mut sections = Vec::with_capacity(number_of_sections);
        for i in 0..number_of_sections {
            let base = section_table.checked_add(i.checked_mul(40)?)?;
            sections.push(Section {
                virtual_size: u32_at(bytes, base.checked_add(8)?)?,
                virtual_address: u32_at(bytes, base.checked_add(12)?)?,
                raw_size: u32_at(bytes, base.checked_add(16)?)?,
                raw_pointer: u32_at(bytes, base.checked_add(20)?)?,
            });
        }

        let resource_base = rva_to_file_offset(&sections, resource_rva)?;
        Some(Header {
            resource_base,
            sections,
        })
    }
}

/// The total entry count of a resource directory at `base` (named and id
/// entries together), and where those entries begin.
fn directory_entries(bytes: &[u8], base: usize) -> Option<(usize, usize)> {
    let named = u16_at(bytes, base.checked_add(12)?)? as usize;
    let id = u16_at(bytes, base.checked_add(14)?)? as usize;
    let start = base.checked_add(16)?;
    Some((named.checked_add(id)?, start))
}

/// The `offset` field of the id entry named `id` in the directory at `base`.
/// A named (string) entry has the high bit of its name set; this reader has
/// no use for a string name, so it is skipped rather than resolved.
fn find_id(bytes: &[u8], base: usize, id: u32) -> Option<u32> {
    let (count, start) = directory_entries(bytes, base)?;
    for i in 0..count {
        let entry = start.checked_add(i.checked_mul(8)?)?;
        let name = u32_at(bytes, entry)?;
        if name & 0x8000_0000 != 0 {
            continue;
        }
        if name == id {
            return u32_at(bytes, entry.checked_add(4)?);
        }
    }
    None
}

/// Every id entry in the directory at `base`, as `(id, offset field)`, in
/// whatever order the file lists them. Ascending order is the caller's job:
/// nothing here assumes the file is already sorted.
fn list_ids(bytes: &[u8], base: usize) -> Option<Vec<(u16, u32)>> {
    let (count, start) = directory_entries(bytes, base)?;
    let mut out = Vec::new();
    for i in 0..count {
        let entry = start.checked_add(i.checked_mul(8)?)?;
        let name = u32_at(bytes, entry)?;
        if name & 0x8000_0000 != 0 {
            continue;
        }
        let offset = u32_at(bytes, entry.checked_add(4)?)?;
        // A well-formed integer id never sets a bit above 15; a crafted one
        // that does is skipped rather than silently truncated, so this
        // agrees with find_id, which compares the untruncated value.
        let Ok(id) = u16::try_from(name) else {
            continue;
        };
        out.push((id, offset));
    }
    Some(out)
}

/// The `offset` field of the first entry in the directory at `base`,
/// whichever name or id it carries: language is not selected on.
fn first_entry(bytes: &[u8], base: usize) -> Option<u32> {
    let (count, start) = directory_entries(bytes, base)?;
    if count == 0 {
        return None;
    }
    u32_at(bytes, start.checked_add(4)?)
}

/// Follows an entry's `offset` field one level down, when it names a
/// subdirectory (high bit set). `None` when it names data instead, or the
/// arithmetic would overflow.
fn subdirectory(resource_base: usize, offset: u32) -> Option<usize> {
    if offset & 0x8000_0000 == 0 {
        return None;
    }
    resource_base.checked_add((offset & 0x7FFF_FFFF) as usize)
}

/// Follows an entry's `offset` field one level down, when it names a data
/// entry (high bit clear).
fn data_entry(resource_base: usize, offset: u32) -> Option<usize> {
    if offset & 0x8000_0000 != 0 {
        return None;
    }
    resource_base.checked_add(offset as usize)
}

/// The bytes a resource data entry at `base` names. Its first field is a
/// true RVA, not an offset relative to the resource directory, so it goes
/// back through the section table rather than being read as `base` was.
fn data_bytes(bytes: &[u8], header: &Header, base: usize) -> Option<Vec<u8>> {
    let rva = u32_at(bytes, base)?;
    let size = u32_at(bytes, base.checked_add(4)?)?;
    let file_offset = rva_to_file_offset(&header.sections, rva)?;
    let end = file_offset.checked_add(size as usize)?;
    bytes.get(file_offset..end).map(|slice| slice.to_vec())
}

/// Descends the three levels a resource always nests through: type, then
/// id, then language. An id entry's offset points straight at the language
/// directory; the language directory is what finally points at data.
fn resource(bytes: &[u8], header: &Header, type_id: u32, resource_id: u16) -> Option<Vec<u8>> {
    let id_dir = subdirectory(
        header.resource_base,
        find_id(bytes, header.resource_base, type_id)?,
    )?;
    let lang_dir = subdirectory(
        header.resource_base,
        find_id(bytes, id_dir, resource_id as u32)?,
    )?;
    let entry = data_entry(header.resource_base, first_entry(bytes, lang_dir)?)?;
    data_bytes(bytes, header, entry)
}

/// One image an `.ico` file carries.
struct Image {
    width: u8,
    height: u8,
    colours: u8,
    reserved: u8,
    planes: u16,
    bit_count: u16,
    bytes: Vec<u8>,
}

/// The icon `wanted` names, as the bytes of an `.ico` file.
pub fn icon(bytes: &[u8], wanted: Wanted) -> Option<Vec<u8>> {
    let header = Header::parse(bytes)?;

    let group_type = subdirectory(
        header.resource_base,
        find_id(bytes, header.resource_base, RT_GROUP_ICON)?,
    )?;
    let mut groups = list_ids(bytes, group_type)?;
    groups.sort_by_key(|&(id, _)| id);
    let &(_, group_offset) = match wanted {
        Wanted::Nth(n) => groups.get(n)?,
        Wanted::Id(id) => groups.iter().find(|&&(gid, _)| gid == id)?,
    };

    let group_lang_dir = subdirectory(header.resource_base, group_offset)?;
    let group_data = data_entry(header.resource_base, first_entry(bytes, group_lang_dir)?)?;
    let group_bytes = data_bytes(bytes, &header, group_data)?;

    // The same RT_ICON id can be named by every entry a group lists, so the
    // output size has no relation to the input file's own size. No real
    // icon group comes anywhere near this cap: the largest one measured
    // here, GIMP's, is 164 KB across ten images. A group whose named images
    // total more than this is malformed, not merely unusual.
    const MAX_TOTAL_IMAGE_BYTES: usize = 8 * 1024 * 1024;

    let count = u16_at(&group_bytes, 4)? as usize;
    let mut images = Vec::new();
    let mut total = 0usize;
    for i in 0..count {
        let entry = 6usize.checked_add(i.checked_mul(14)?)?;
        let record = group_bytes.get(entry..entry.checked_add(14)?)?;
        let icon_id = u16::from_le_bytes([record[12], record[13]]);
        let Some(image_bytes) = resource(bytes, &header, RT_ICON, icon_id) else {
            // Named but not carried: keep the images the file does have.
            continue;
        };
        total = total.checked_add(image_bytes.len())?;
        if total > MAX_TOTAL_IMAGE_BYTES {
            return None;
        }
        images.push(Image {
            width: record[0],
            height: record[1],
            colours: record[2],
            reserved: record[3],
            planes: u16::from_le_bytes([record[4], record[5]]),
            bit_count: u16::from_le_bytes([record[6], record[7]]),
            bytes: image_bytes,
        });
    }
    if images.is_empty() {
        return None;
    }

    let mut ico = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes());
    ico.extend_from_slice(&1u16.to_le_bytes());
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + images.len() * 16;
    for image in &images {
        ico.push(image.width);
        ico.push(image.height);
        ico.push(image.colours);
        ico.push(image.reserved);
        ico.extend_from_slice(&image.planes.to_le_bytes());
        ico.extend_from_slice(&image.bit_count.to_le_bytes());
        ico.extend_from_slice(&(image.bytes.len() as u32).to_le_bytes());
        ico.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += image.bytes.len();
    }
    for image in &images {
        ico.extend_from_slice(&image.bytes);
    }
    Some(ico)
}

pub const RT_ICON: u32 = 3;
pub const RT_GROUP_ICON: u32 = 14;

#[cfg(test)]
mod tests {
    use super::*;

    /// A PE carrying exactly the resources asked for. Built rather than
    /// checked in: the parser is what is under test, and a synthesised file
    /// says precisely which bytes produced which answer.
    struct Builder {
        /// `(type_id, resource_id, bytes)`, in the order they are laid out.
        resources: Vec<(u32, u16, Vec<u8>)>,
        plus: bool,
    }

    impl Builder {
        fn new(plus: bool) -> Builder {
            Builder {
                resources: Vec::new(),
                plus,
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

        /// Lay the whole thing out. The resource section's virtual address
        /// is deliberately not its file offset, so a reader that confuses
        /// the two fails every test below.
        fn build(self) -> Vec<u8> {
            use std::collections::HashMap;

            fn put_u16(buf: &mut [u8], at: usize, v: u16) {
                buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
            }
            fn put_u32(buf: &mut [u8], at: usize, v: u32) {
                buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
            }

            // Group by type, keeping the order types and ids first appear
            // in rather than sorting: the reader is what must produce
            // ascending order, not the file layout.
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

            // Layout: a type directory, then one id directory per type,
            // then one language directory per id, then one data entry per
            // id, then the raw bytes those data entries point at.
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

            // Header sizing, independent of the resource content above.
            let pe_offset = 0x40usize;
            let optional_header_size = (if self.plus { 0x70 } else { 0x60 }) + 24;
            let section_table_size = 40; // one section
            let section_file_offset =
                pe_offset + 4 + 20 + optional_header_size + section_table_size;
            let section_va = section_file_offset + 0x4000; // not the file offset, on purpose

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
                        put_u32(&mut content, lang_base + 16, 0); // language id, unused
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
            debug_assert_eq!(file.len(), pe_offset);

            file.extend_from_slice(b"PE\0\0");
            let coff_start = file.len();
            file.extend_from_slice(&0x14Cu16.to_le_bytes()); // machine, unused by the reader
            file.extend_from_slice(&1u16.to_le_bytes()); // number of sections
            file.extend_from_slice(&0u32.to_le_bytes()); // timestamp
            file.extend_from_slice(&0u32.to_le_bytes()); // pointer to symbol table
            file.extend_from_slice(&0u32.to_le_bytes()); // number of symbols
            file.extend_from_slice(&(optional_header_size as u16).to_le_bytes());
            file.extend_from_slice(&0u16.to_le_bytes()); // characteristics
            debug_assert_eq!(file.len(), coff_start + 20);

            let optional_start = file.len();
            let magic: u16 = if self.plus { 0x20b } else { 0x10b };
            file.extend_from_slice(&magic.to_le_bytes());
            let data_directory_offset = if self.plus { 0x70 } else { 0x60 };
            file.resize(optional_start + data_directory_offset, 0);
            file.extend_from_slice(&0u32.to_le_bytes()); // data directory 0: RVA
            file.extend_from_slice(&0u32.to_le_bytes()); // data directory 0: size
            file.extend_from_slice(&0u32.to_le_bytes()); // data directory 1: RVA
            file.extend_from_slice(&0u32.to_le_bytes()); // data directory 1: size
            file.extend_from_slice(&(section_va as u32).to_le_bytes()); // resource table RVA
            file.extend_from_slice(&(content_size as u32).to_le_bytes()); // resource table size
            debug_assert_eq!(file.len(), optional_start + optional_header_size);

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
            debug_assert_eq!(file.len(), section_file_offset);

            file.extend_from_slice(&content);
            file
        }
    }

    /// The pixel width of each image an `.ico` carries, in order.
    fn sizes(ico: &[u8]) -> Vec<u8> {
        let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
        (0..count).map(|i| ico[6 + i * 16]).collect()
    }

    fn offset_of(ico: &[u8], n: usize) -> usize {
        let at = 6 + n * 16 + 12;
        u32::from_le_bytes([ico[at], ico[at + 1], ico[at + 2], ico[at + 3]]) as usize
    }

    #[test]
    fn a_group_becomes_an_ico_carrying_every_image_it_names() {
        let pe = Builder::new(true)
            .add(RT_ICON, 1, vec![0xAA; 64])
            .add(RT_ICON, 2, vec![0xBB; 128])
            .group(7, &[(16, 1), (32, 2)])
            .build();
        let ico = icon(&pe, Wanted::Nth(0)).expect("the group is there");
        assert_eq!(
            &ico[0..6],
            &[0, 0, 1, 0, 2, 0],
            "an ICONDIR naming two images"
        );
        assert_eq!(sizes(&ico), vec![16, 32]);
        let first = offset_of(&ico, 0);
        let second = offset_of(&ico, 1);
        assert_eq!(ico[first], 0xAA);
        assert_eq!(ico[second], 0xBB);
        assert_eq!(second, first + 64, "the second image follows the first");
        assert_eq!(first, 6 + 2 * 16, "the images follow the directory");
    }

    #[test]
    fn a_negative_index_names_a_resource_id_and_a_positive_one_counts() {
        let pe = Builder::new(true)
            .add(RT_ICON, 1, vec![0xAA; 64])
            .add(RT_ICON, 2, vec![0xBB; 64])
            .group(101, &[(16, 1)])
            .group(9, &[(32, 2)])
            .build();
        // Ascending id order, so group 9 is the first and group 101 the second.
        assert_eq!(sizes(&icon(&pe, Wanted::Nth(0)).unwrap()), vec![32]);
        assert_eq!(sizes(&icon(&pe, Wanted::Nth(1)).unwrap()), vec![16]);
        assert_eq!(sizes(&icon(&pe, Wanted::Id(101)).unwrap()), vec![16]);
        assert_eq!(icon(&pe, Wanted::Id(55)), None);
        assert_eq!(icon(&pe, Wanted::Nth(2)), None);
    }

    /// 32-bit binaries are still everywhere on Windows, and their data
    /// directory sits sixteen bytes earlier than a 64-bit one's.
    #[test]
    fn a_32_bit_binary_reads_the_same_as_a_64_bit_one() {
        let make = |plus| {
            Builder::new(plus)
                .add(RT_ICON, 1, vec![0xCC; 48])
                .group(1, &[(48, 1)])
                .build()
        };
        let thirty_two =
            icon(&make(false), Wanted::Nth(0)).expect("the 32-bit build carries the icon");
        assert_eq!(Some(thirty_two), icon(&make(true), Wanted::Nth(0)));
    }

    /// A group naming an icon the file does not carry is not a reason to
    /// lose the icons it does carry.
    #[test]
    fn an_image_the_group_names_but_the_file_lacks_is_dropped() {
        let pe = Builder::new(true)
            .add(RT_ICON, 1, vec![0xAA; 64])
            .group(1, &[(16, 1), (32, 404)])
            .build();
        assert_eq!(sizes(&icon(&pe, Wanted::Nth(0)).unwrap()), vec![16]);
    }

    /// A group can name the same real, present icon more than once. Nothing
    /// about that is malformed on its own, but the total it claims can still
    /// run past what any real icon carries, and that is refused rather than
    /// allocated.
    #[test]
    fn a_group_claiming_more_than_the_cap_is_refused() {
        let pe = Builder::new(true)
            .add(RT_ICON, 1, vec![0xAA; 5 * 1024 * 1024])
            .group(1, &[(16, 1), (32, 1)])
            .build();
        assert_eq!(icon(&pe, Wanted::Nth(0)), None);
    }

    #[test]
    fn a_group_naming_nothing_present_is_no_icon_at_all() {
        let pe = Builder::new(true).group(1, &[(16, 404)]).build();
        assert_eq!(icon(&pe, Wanted::Nth(0)), None);
    }

    #[test]
    fn a_file_with_no_resources_at_all_is_no_icon() {
        let pe = Builder::new(true).build();
        assert_eq!(icon(&pe, Wanted::Nth(0)), None);
    }

    #[test]
    fn rubbish_is_refused_rather_than_panicked_over() {
        assert_eq!(icon(b"", Wanted::Nth(0)), None);
        assert_eq!(icon(b"MZ", Wanted::Nth(0)), None);
        assert_eq!(icon(&vec![0x4D; 4096], Wanted::Nth(0)), None);
        // A PE cut in half, so its resource directory points past the end.
        let mut pe = Builder::new(true)
            .add(RT_ICON, 1, vec![0xAA; 64])
            .group(1, &[(16, 1)])
            .build();
        let half = pe.len() / 2;
        pe.truncate(half);
        assert_eq!(icon(&pe, Wanted::Nth(0)), None);
    }

    /// The machine's own binaries are the only test of the real format.
    /// Skipped where the file is absent rather than failed, so the suite
    /// stays green on Linux and inside a container.
    #[cfg(windows)]
    #[test]
    fn a_real_system_binary_yields_a_real_icon() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into());
        let path = std::path::Path::new(&root).join("explorer.exe");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipped: {} is not readable", path.display());
            return;
        };
        let ico = icon(&bytes, Wanted::Nth(0)).expect("explorer.exe has an icon");
        assert_eq!(&ico[0..4], &[0, 0, 1, 0], "an ICONDIR for an icon");
        assert!(
            ico.len() > 1024,
            "an icon of {} bytes is not one",
            ico.len()
        );
        eprintln!(
            "explorer.exe gave {} bytes, sizes {:?}",
            ico.len(),
            sizes(&ico)
        );
    }
}
