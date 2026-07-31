/*
File: crates/ms-fonts/src/family_name.rs

Purpose:
Reads the family name of a font file out of its `name` table, without loading the file
into memory: only the sfnt table directory and the `name` table itself are read.

Main responsibilities:
- locate the `name` table (including inside a TrueType collection);
- pick the family name the way `fontdb` does, so a name handed to a shaper matches the
  name the same file is registered under.

Key functions:
- `read_family_name`: the family name of the first face of a font file.

Notes:
Only a few kilobytes per file are read, which is what lets the manifest describe the
large `ext` tier without paying for its ~80 MB. `fontdb` does the same when registering
a `Source::File` — it maps the file only to read this table
(`fontdb-0.16.2/src/lib.rs:264-274`).
*/

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use ms_log::runtime_log;
use ttf_parser::Language;
use ttf_parser::name::Names;

/// Magic of the first four bytes of a TrueType collection (`.ttc` / `.otc`).
const COLLECTION_TAG: &[u8; 4] = b"ttcf";
/// Table tag of the OpenType naming table.
const NAME_TAG: &[u8; 4] = b"name";
/// Size of the sfnt offset table: version, `numTables`, and three search hints.
const OFFSET_TABLE_LEN: usize = 12;
/// Size of one entry of the sfnt table directory: tag, checksum, offset, length.
const TABLE_RECORD_LEN: usize = 16;
/// Upper bound accepted for the `name` table; real ones are a few kilobytes.
///
/// Guards the single allocation this module makes against a corrupt or truncated file
/// claiming a table of gigabytes.
const MAX_NAME_TABLE_LEN: usize = 4 * 1024 * 1024;

/// Returns the family name of the first face in `path`, or `None` with a logged reason.
///
/// The selection rule mirrors `fontdb::parse_names` (`fontdb-0.16.2/src/lib.rs:971-1006`):
/// the typographic family (name ID 16) if the font has one, otherwise the family (name
/// ID 1), preferring the US-English record. Matching that rule is what makes the returned
/// name usable as a cosmic-text fallback family for the very same file.
///
/// For a collection only face 0 is described: a stack entry is one file with one name.
pub(crate) fn read_family_name(path: &Path) -> Option<String> {
    let table_data = read_name_table(path)?;
    let Some(table) = ttf_parser::name::Table::parse(&table_data) else {
        runtime_log::log_warn(format!(
            "[ms_fonts] the `name` table of '{}' could not be parsed; the file cannot be \
             addressed by family name",
            path.display()
        ));
        return None;
    };

    let family = select_family(table.names);
    if family.is_none() {
        runtime_log::log_warn(format!(
            "[ms_fonts] the `name` table of '{}' holds no unicode family-name record; the \
             file cannot be addressed by family name",
            path.display()
        ));
    }
    family
}

/// Picks the family name out of a parsed `name` table.
///
/// Prefers the typographic family over the plain one, and the US-English record over any
/// other language, so the result is stable regardless of record order in the file.
fn select_family(names: Names<'_>) -> Option<String> {
    let mut families = collect_families(ttf_parser::name_id::TYPOGRAPHIC_FAMILY, names);
    if families.is_empty() {
        families = collect_families(ttf_parser::name_id::FAMILY, names);
    }

    if let Some(index) = families
        .iter()
        .position(|(_, language)| *language == Language::English_UnitedStates)
    {
        return Some(families.swap_remove(index).0);
    }
    families.into_iter().next().map(|(family, _)| family)
}

/// Collects every non-empty unicode record of `name_id`, with the language it declares.
///
/// Non-unicode encodings are skipped: `ttf-parser` can only decode unicode records
/// (`ttf-parser-0.25.1/src/tables/name.rs:145-153`), and every font in the bundle carries
/// its family name in the Windows/Unicode encoding.
fn collect_families(name_id: u16, names: Names<'_>) -> Vec<(String, Language)> {
    names
        .into_iter()
        .filter(|name| name.name_id == name_id && name.is_unicode())
        .filter_map(|name| {
            let family = name.to_string()?;
            if family.is_empty() {
                return None;
            }
            Some((family, name.language()))
        })
        .collect()
}

/// Reads the raw bytes of the `name` table of the first face in `path`.
///
/// Walks the sfnt structure by hand — collection header (if any), offset table, table
/// directory — and reads only the table itself, so describing a 20 MB font costs a few
/// kilobytes. Every failing step is logged with its operation and returns `None`.
fn read_name_table(path: &Path) -> Option<Vec<u8>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) => return read_failed(path, "opening the file", &err),
    };

    let mut offset_table = [0u8; OFFSET_TABLE_LEN];
    if let Err(err) = file.read_exact(&mut offset_table) {
        return read_failed(path, "reading the offset table", &err);
    }

    if &offset_table[..4] == COLLECTION_TAG {
        // TrueType collection: the 12-byte collection header is followed by one 32-bit
        // file offset per face; the first one points at the offset table of face 0.
        let mut first_face_offset = [0u8; 4];
        if let Err(err) = file.read_exact(&mut first_face_offset) {
            return read_failed(path, "reading the collection offsets", &err);
        }
        let face_offset = u64::from(u32::from_be_bytes(first_face_offset));
        if let Err(err) = file.seek(SeekFrom::Start(face_offset)) {
            return read_failed(path, "seeking to the first face", &err);
        }
        if let Err(err) = file.read_exact(&mut offset_table) {
            return read_failed(path, "reading the offset table of the first face", &err);
        }
    }

    // `numTables` is bounded by u16, so the directory is at most 1 MiB even if the file
    // is corrupt; the read below fails long before that on a truncated file.
    let table_count = u16::from_be_bytes([offset_table[4], offset_table[5]]);
    let mut directory = vec![0u8; usize::from(table_count) * TABLE_RECORD_LEN];
    if let Err(err) = file.read_exact(&mut directory) {
        return read_failed(path, "reading the table directory", &err);
    }

    let Some((table_offset, table_len)) = find_name_record(&directory) else {
        runtime_log::log_warn(format!(
            "[ms_fonts] font file '{}' has no `name` table; the file cannot be addressed \
             by family name",
            path.display()
        ));
        return None;
    };

    let Ok(table_len) = usize::try_from(table_len) else {
        runtime_log::log_warn(format!(
            "[ms_fonts] font file '{}' declares a `name` table of {table_len} bytes, which \
             does not fit this platform's address space; the file is skipped",
            path.display()
        ));
        return None;
    };
    if table_len == 0 || table_len > MAX_NAME_TABLE_LEN {
        runtime_log::log_warn(format!(
            "[ms_fonts] font file '{}' declares an implausible `name` table of {table_len} \
             bytes (accepted: 1..={MAX_NAME_TABLE_LEN}); the file is skipped",
            path.display()
        ));
        return None;
    }

    if let Err(err) = file.seek(SeekFrom::Start(u64::from(table_offset))) {
        return read_failed(path, "seeking to the `name` table", &err);
    }
    let mut table_data = vec![0u8; table_len];
    if let Err(err) = file.read_exact(&mut table_data) {
        return read_failed(path, "reading the `name` table", &err);
    }
    Some(table_data)
}

/// Returns the `(offset, length)` of the `name` table from an sfnt table directory.
///
/// `directory` is a flat run of 16-byte records; a partial trailing record is ignored.
fn find_name_record(directory: &[u8]) -> Option<(u32, u32)> {
    directory
        .chunks_exact(TABLE_RECORD_LEN)
        .find(|record| record.starts_with(NAME_TAG))
        .and_then(|record| {
            let offset = u32::from_be_bytes(record.get(8..12)?.try_into().ok()?);
            let length = u32::from_be_bytes(record.get(12..16)?.try_into().ok()?);
            Some((offset, length))
        })
}

/// Logs one failed step of the `name`-table read and returns `None`.
///
/// Generic in the return type only so a caller can `return read_failed(..)` directly.
fn read_failed<T>(path: &Path, operation: &str, err: &std::io::Error) -> Option<T> {
    runtime_log::log_warn(format!(
        "[ms_fonts] cannot read the `name` table of '{}': {operation} failed: {err}; the \
         file is left out of the font stack",
        path.display()
    ));
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// One `name` record to encode: name ID, language ID, and the text itself.
    struct NameEntry {
        name_id: u16,
        language_id: u16,
        text: &'static str,
    }

    /// Builds a format-0 `name` table with Windows/Unicode-BMP records.
    fn build_name_table(entries: &[NameEntry]) -> Vec<u8> {
        let record_count = u16::try_from(entries.len()).expect("test tables stay small");
        let storage_offset = 6 + record_count * 12;

        let mut records: Vec<u8> = Vec::new();
        let mut storage: Vec<u8> = Vec::new();
        for entry in entries {
            let encoded: Vec<u8> = entry
                .text
                .encode_utf16()
                .flat_map(u16::to_be_bytes)
                .collect();
            let offset = u16::try_from(storage.len()).expect("test storage stays small");
            let length = u16::try_from(encoded.len()).expect("test names stay short");

            // platformID = 3 (Windows), encodingID = 1 (Unicode BMP).
            records.extend_from_slice(&3u16.to_be_bytes());
            records.extend_from_slice(&1u16.to_be_bytes());
            records.extend_from_slice(&entry.language_id.to_be_bytes());
            records.extend_from_slice(&entry.name_id.to_be_bytes());
            records.extend_from_slice(&length.to_be_bytes());
            records.extend_from_slice(&offset.to_be_bytes());
            storage.extend_from_slice(&encoded);
        }

        let mut table: Vec<u8> = Vec::new();
        table.extend_from_slice(&0u16.to_be_bytes()); // format 0
        table.extend_from_slice(&record_count.to_be_bytes());
        table.extend_from_slice(&storage_offset.to_be_bytes());
        table.extend_from_slice(&records);
        table.extend_from_slice(&storage);
        table
    }

    /// Builds a single-face sfnt whose only table is the given `name` table.
    ///
    /// `base_offset` is where the face itself starts inside the final file: table offsets
    /// in the directory are absolute file offsets, which for a face inside a collection is
    /// not the same as its offset within the face.
    fn build_font_at(name_table: &[u8], base_offset: u32) -> Vec<u8> {
        let table_offset = u32::try_from(OFFSET_TABLE_LEN + TABLE_RECORD_LEN)
            .expect("the sfnt header is 28 bytes")
            + base_offset;
        let table_len = u32::try_from(name_table.len()).expect("test tables stay small");

        let mut font: Vec<u8> = Vec::new();
        font.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // sfnt version 1.0
        font.extend_from_slice(&1u16.to_be_bytes()); // numTables
        font.extend_from_slice(&[0u8; 6]); // searchRange, entrySelector, rangeShift
        font.extend_from_slice(NAME_TAG);
        font.extend_from_slice(&0u32.to_be_bytes()); // checksum (never verified)
        font.extend_from_slice(&table_offset.to_be_bytes());
        font.extend_from_slice(&table_len.to_be_bytes());
        font.extend_from_slice(name_table);
        font
    }

    /// A stand-alone single-face sfnt file.
    fn build_font(name_table: &[u8]) -> Vec<u8> {
        build_font_at(name_table, 0)
    }

    /// Where the first face starts in the collection built by [`build_collection`].
    ///
    /// Collection header (12 bytes) plus one 32-bit offset per face (2 faces).
    fn collection_header_len() -> u32 {
        u32::try_from(OFFSET_TABLE_LEN + 8).expect("the header is 20 bytes")
    }

    /// Wraps a `name` table into a two-entry TrueType collection.
    ///
    /// Both entries point at the same face, which is enough to prove that the collection
    /// header is walked instead of being parsed as an offset table.
    fn build_collection(name_table: &[u8]) -> Vec<u8> {
        let header_len = collection_header_len();
        let face = build_font_at(name_table, header_len);

        let mut collection: Vec<u8> = Vec::new();
        collection.extend_from_slice(COLLECTION_TAG);
        collection.extend_from_slice(&1u16.to_be_bytes()); // majorVersion
        collection.extend_from_slice(&0u16.to_be_bytes()); // minorVersion
        collection.extend_from_slice(&2u32.to_be_bytes()); // numFonts
        collection.extend_from_slice(&header_len.to_be_bytes()); // face 0
        collection.extend_from_slice(&header_len.to_be_bytes()); // face 1 (same face)
        // The face has to start exactly where the offsets above claim, because its own
        // table offsets were built absolute from that position.
        collection.extend_from_slice(&face);
        collection
    }

    /// Writes `bytes` into `dir` under `name` and returns the path.
    fn write_font(dir: &Path, name: &str, bytes: &[u8]) -> Result<std::path::PathBuf, std::io::Error>
    {
        let path = dir.join(name);
        fs::write(&path, bytes)?;
        Ok(path)
    }

    #[test]
    fn the_family_name_is_read_from_the_name_table() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let table = build_name_table(&[NameEntry {
            name_id: ttf_parser::name_id::FAMILY,
            language_id: 0x0409,
            text: "Ms Test Sans",
        }]);
        let path = write_font(dir.path(), "00-Test.ttf", &build_font(&table))?;

        assert_eq!(read_family_name(&path), Some("Ms Test Sans".to_owned()));
        Ok(())
    }

    #[test]
    fn the_typographic_family_wins_over_the_plain_one() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let table = build_name_table(&[
            NameEntry {
                name_id: ttf_parser::name_id::FAMILY,
                language_id: 0x0409,
                text: "Ms Test Sans Light",
            },
            NameEntry {
                name_id: ttf_parser::name_id::TYPOGRAPHIC_FAMILY,
                language_id: 0x0409,
                text: "Ms Test Sans",
            },
        ]);
        let path = write_font(dir.path(), "00-Test.ttf", &build_font(&table))?;

        assert_eq!(read_family_name(&path), Some("Ms Test Sans".to_owned()));
        Ok(())
    }

    #[test]
    fn the_us_english_record_is_preferred_over_other_languages() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let table = build_name_table(&[
            NameEntry {
                name_id: ttf_parser::name_id::FAMILY,
                language_id: 0x0419, // Russian
                text: "Тестовый шрифт",
            },
            NameEntry {
                name_id: ttf_parser::name_id::FAMILY,
                language_id: 0x0409, // US English
                text: "Ms Test Sans",
            },
        ]);
        let path = write_font(dir.path(), "00-Test.ttf", &build_font(&table))?;

        assert_eq!(read_family_name(&path), Some("Ms Test Sans".to_owned()));
        Ok(())
    }

    #[test]
    fn a_collection_is_described_by_its_first_face() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let table = build_name_table(&[NameEntry {
            name_id: ttf_parser::name_id::FAMILY,
            language_id: 0x0409,
            text: "Ms Test Collection",
        }]);
        let path = write_font(dir.path(), "00-Test.ttc", &build_collection(&table))?;

        assert_eq!(read_family_name(&path), Some("Ms Test Collection".to_owned()));
        Ok(())
    }

    #[test]
    fn a_file_that_is_not_a_font_is_rejected_instead_of_guessed() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let path = write_font(dir.path(), "00-NotAFont.ttf", b"plain text, not a font")?;

        assert_eq!(read_family_name(&path), None);
        Ok(())
    }

    #[test]
    fn the_name_record_is_found_in_a_multi_table_directory() {
        let mut directory: Vec<u8> = Vec::new();
        for (tag, offset, length) in [
            (b"cmap", 100u32, 10u32),
            (b"name", 200u32, 20u32),
            (b"post", 300u32, 30u32),
        ] {
            directory.extend_from_slice(tag);
            directory.extend_from_slice(&0u32.to_be_bytes());
            directory.extend_from_slice(&offset.to_be_bytes());
            directory.extend_from_slice(&length.to_be_bytes());
        }

        assert_eq!(find_name_record(&directory), Some((200, 20)));
        assert_eq!(find_name_record(&directory[..TABLE_RECORD_LEN]), None);
        assert_eq!(find_name_record(&[]), None);
    }
}
