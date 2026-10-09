//! A minimal zip archive writer, hand-rolled so the export route needs no
//! archive dependency in the wasm graph.
//!
//! Only what an export needs: **stored** entries (method 0 — the bodies are
//! Markdown, and Obsidian imports a plain zip), UTF-8 names (general-purpose
//! bit 11), version-needed 20, and a fixed 1980-01-01 timestamp so the same
//! wiki zips to the same bytes — timestamps have no business in an export.
//! The format is the one [PKWARE's APPNOTE][] describes: a local file header
//! and the data per entry, one central directory, one end-of-central-directory
//! record.
//!
//! [PKWARE's APPNOTE]: https://pkware.cachefly.net/webdocs/casestudies/APPNOTE.TXT

/// The most entries one archive can carry: the count fields are `u16`.
///
/// The export route never gets near this — it lists at most its page limit
/// (1,000) — so the cap is a guard that turns "impossible" into "no
/// archive" rather than a corrupt zip.
pub(crate) const MAX_ENTRIES: usize = u16::MAX as usize;

/// The local header's "version needed to extract": 2.0, the level stored
/// entries require.
const VERSION_NEEDED: u16 = 20;
/// General-purpose bit 11: the file names are UTF-8, not code-page 437.
const UTF8_FLAG: u16 = 0x0800;
/// Compression method 0: stored, no deflate.
const STORED: u16 = 0;
/// 00:00, DOS time — the hour half of the fixed timestamp.
const DOS_TIME: u16 = 0;
/// 1980-01-01, DOS date — the earliest date the format can express, chosen
/// so exports are byte-identical however long the wiki has existed. Year
/// 1980 contributes nothing to the bit layout (`(year - 1980) << 9`), so
/// the value is the month and day alone.
const DOS_DATE: u16 = 1 << 5 | 1;

const LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const DIRECTORY_SIGNATURE: u32 = 0x0201_4b50;
const EOCD_SIGNATURE: u32 = 0x0605_4b50;

/// Builds one zip archive from `(name, bytes)` pairs, or `None` when the
/// entries would not fit the format's `u16` count fields.
///
/// Names are written as UTF-8 with bit 11 set; contents are stored
/// uncompressed, each guarded by its CRC-32.
pub(crate) fn archive(entries: &[(String, Vec<u8>)]) -> Option<Vec<u8>> {
    if entries.len() > MAX_ENTRIES {
        return None;
    }
    // Names come from `{scope}/{slug}.md`, and both halves pass the store's
    // slug rule, so a `u16` name length is a given; this writer has no
    // other caller.
    let mut out = Vec::new();
    let mut directory: Vec<(String, u32, u32, u32)> = Vec::with_capacity(entries.len());
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        push_u32(&mut out, LOCAL_SIGNATURE);
        push_u16(&mut out, VERSION_NEEDED);
        push_u16(&mut out, UTF8_FLAG);
        push_u16(&mut out, STORED);
        push_u16(&mut out, DOS_TIME);
        push_u16(&mut out, DOS_DATE);
        push_u32(&mut out, crc);
        push_u32(&mut out, data.len() as u32);
        push_u32(&mut out, data.len() as u32);
        push_u16(&mut out, name.len() as u16);
        push_u16(&mut out, 0); // no extra field
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        directory.push((name.clone(), crc, data.len() as u32, offset));
    }

    let directory_offset = out.len() as u32;
    for (name, crc, size, offset) in &directory {
        push_u32(&mut out, DIRECTORY_SIGNATURE);
        push_u16(&mut out, VERSION_NEEDED); // version made by
        push_u16(&mut out, VERSION_NEEDED);
        push_u16(&mut out, UTF8_FLAG);
        push_u16(&mut out, STORED);
        push_u16(&mut out, DOS_TIME);
        push_u16(&mut out, DOS_DATE);
        push_u32(&mut out, *crc);
        push_u32(&mut out, *size);
        push_u32(&mut out, *size);
        push_u16(&mut out, name.len() as u16);
        push_u16(&mut out, 0); // no extra field
        push_u16(&mut out, 0); // no comment
        push_u16(&mut out, 0); // starts on this disk
        push_u16(&mut out, 0); // internal attributes
        push_u32(&mut out, 0); // external attributes
        push_u32(&mut out, *offset);
        out.extend_from_slice(name.as_bytes());
    }
    let directory_size = out.len() - directory_offset as usize;

    push_u32(&mut out, EOCD_SIGNATURE);
    push_u16(&mut out, 0); // this disk
    push_u16(&mut out, 0); // the disk the directory starts on
    push_u16(&mut out, directory.len() as u16);
    push_u16(&mut out, directory.len() as u16);
    push_u32(&mut out, directory_size as u32);
    push_u32(&mut out, directory_offset);
    push_u16(&mut out, 0); // no archive comment
    Some(out)
}

/// CRC-32/IEEE (the zip polynomial, `0xEDB88320` reflected), computed
/// bit-by-bit — no table, no dependency.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let low = crc & 1;
            crc >>= 1;
            if low == 1 {
                crc ^= 0xEDB8_8320;
            }
        }
    }
    !crc
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::{
        DOS_DATE, DOS_TIME, EOCD_SIGNATURE, LOCAL_SIGNATURE, MAX_ENTRIES, STORED, UTF8_FLAG,
        VERSION_NEEDED, archive, crc32,
    };

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(bytes[at..at + 2].try_into().expect("u16 field"))
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().expect("u32 field"))
    }

    /// A minimal zip reader, test-side only: walks the end-of-central-
    /// directory record, then the central directory, then each local header,
    /// and verifies every stored CRC along the way. The tests below pin that
    /// what the writer builds is what a real unzip reads.
    fn read(archive_bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        // No archive comment, so the record is exactly the last 22 bytes.
        let eocd = archive_bytes.len() - 22;
        assert_eq!(u32_at(archive_bytes, eocd), EOCD_SIGNATURE, "EOCD last");
        let entries = u16_at(archive_bytes, eocd + 10) as usize;
        let directory_size = u32_at(archive_bytes, eocd + 12) as usize;
        let directory_offset = u32_at(archive_bytes, eocd + 16) as usize;

        let mut at = directory_offset;
        let directory_end = directory_offset + directory_size;
        let mut found = Vec::with_capacity(entries);
        for _ in 0..entries {
            assert_eq!(
                u32_at(archive_bytes, at),
                super::DIRECTORY_SIGNATURE,
                "central directory entry signature"
            );
            let crc = u32_at(archive_bytes, at + 16);
            let size = u32_at(archive_bytes, at + 24) as usize;
            let name_len = u16_at(archive_bytes, at + 28) as usize;
            let extra_len = u16_at(archive_bytes, at + 30) as usize;
            let comment_len = u16_at(archive_bytes, at + 32) as usize;
            let offset = u32_at(archive_bytes, at + 42) as usize;
            let name = &archive_bytes[at + 46..at + 46 + name_len];

            let local = offset;
            assert_eq!(
                u32_at(archive_bytes, local),
                LOCAL_SIGNATURE,
                "local header signature"
            );
            let local_name_len = u16_at(archive_bytes, local + 26) as usize;
            let local_extra_len = u16_at(archive_bytes, local + 28) as usize;
            let data_start = local + 30 + local_name_len + local_extra_len;
            let data = &archive_bytes[data_start..data_start + size];
            assert_eq!(crc32(data), crc, "stored CRC matches the data");

            found.push((
                String::from_utf8(name.to_vec()).expect("UTF-8 name"),
                data.to_vec(),
            ));
            at += 46 + name_len + extra_len + comment_len;
        }
        assert_eq!(at, directory_end, "the directory is exactly its entries");
        found
    }

    #[test]
    fn the_crc_matches_the_check_value() {
        // The standard CRC-32/IEEE check string.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn the_archive_round_trips_through_a_reader() {
        let entries = vec![
            (
                "t0smoketest/zebra-routes.md".to_owned(),
                b"---\ntitle: Zebra routes\n---\n\nBody with a [link].\n".to_vec(),
            ),
            ("t0smoketest/empty.md".to_owned(), Vec::new()),
        ];
        let bytes = archive(&entries).expect("an archive");
        assert_eq!(read(&bytes), entries);
    }

    #[test]
    fn two_entries_with_non_ascii_names_round_trip() {
        let entries = vec![
            (
                "t0smoketest/scène/naïve-ø.md".to_owned(),
                "contenu naïve — ✓".as_bytes().to_vec(),
            ),
            (
                "t0smoketest/日本語ページ.md".to_owned(),
                "日本語の本文".as_bytes().to_vec(),
            ),
        ];
        let bytes = archive(&entries).expect("an archive");
        assert_eq!(read(&bytes), entries);
    }

    #[test]
    fn an_empty_wiki_is_a_valid_empty_zip() {
        let bytes = archive(&[]).expect("an archive");
        // Just the end-of-central-directory record.
        assert_eq!(bytes.len(), 22);
        assert_eq!(read(&bytes), Vec::new());
    }

    #[test]
    fn the_headers_carry_what_readers_expect() {
        let bytes =
            archive(&[("t0smoketest/one.md".to_owned(), b"one".to_vec())]).expect("an archive");
        // Local header: version 20, UTF-8 flag, stored, fixed DOS stamp.
        assert_eq!(u16_at(&bytes, 4), VERSION_NEEDED);
        assert_eq!(u16_at(&bytes, 6), UTF8_FLAG);
        assert_eq!(u16_at(&bytes, 8), STORED);
        assert_eq!(u16_at(&bytes, 10), DOS_TIME);
        assert_eq!(u16_at(&bytes, 12), DOS_DATE);
        // Central directory: the same stamp, right after its signature.
        let directory_offset = u32_at(&bytes, bytes.len() - 22 + 16) as usize;
        assert_eq!(u16_at(&bytes, directory_offset + 8), UTF8_FLAG);
        assert_eq!(u16_at(&bytes, directory_offset + 10), STORED);
        assert_eq!(u16_at(&bytes, directory_offset + 12), DOS_TIME);
        assert_eq!(u16_at(&bytes, directory_offset + 14), DOS_DATE);
    }

    #[test]
    fn the_entry_cap_is_enforced_rather_than_corrupting_the_count() {
        let many: Vec<(String, Vec<u8>)> = (0..MAX_ENTRIES + 1)
            .map(|index| (format!("t0smoketest/page-{index:05}.md"), Vec::new()))
            .collect();
        assert!(archive(&many).is_none(), "too many entries: no archive");
        let mut many = many;
        many.truncate(MAX_ENTRIES);
        let bytes = archive(&many).expect("an archive at the cap");
        assert_eq!(read(&bytes).len(), MAX_ENTRIES);
    }
}
