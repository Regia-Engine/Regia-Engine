// SPDX-License-Identifier: MPL-2.0
//! A minimal ZIP archive writer and reader: stored entries only (no compression), UTF-8 names,
//! no ZIP64. Written from the public PKWARE `APPNOTE.TXT` (§ 4.3 file structure, § 4.4 fields);
//! the CRC-32 is the IEEE 802.3 polynomial (reflected `0xEDB88320`).
//!
//! A feedback bundle is a handful of small text files and one PNG (already compressed), so
//! storing is cheap and every OS opens the archive without extra tools.

use regia_core::log::civil_date_time;

const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const END_SIG: u32 = 0x0605_4b50;
/// Version 2.0: the lowest that names stored entries and directories.
const VERSION: u16 = 20;
/// General-purpose flag bit 11: names are UTF-8.
const FLAG_UTF8: u16 = 0x0800;

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32 (IEEE) of `data`, as ZIP stores it.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// MS-DOS time and date fields for `unix_secs` (UTC; ZIP has no zone).
fn dos_time(unix_secs: u64) -> (u16, u16) {
    let (y, mo, d, h, mi, s) = civil_date_time(unix_secs);
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | ((s as u16) / 2);
    let year = (y - 1980).clamp(0, 127) as u16;
    let date = (year << 9) | ((mo as u16) << 5) | d as u16;
    (time, date)
}

/// Builds a ZIP archive in memory.
#[derive(Debug, Default)]
pub struct ZipWriter {
    out: Vec<u8>,
    central: Vec<u8>,
    count: u16,
    time: u16,
    date: u16,
}

impl ZipWriter {
    /// An empty archive whose entries carry the modification time `unix_secs`.
    #[must_use]
    pub fn new(unix_secs: u64) -> Self {
        let (time, date) = dos_time(unix_secs);
        Self {
            time,
            date,
            ..Self::default()
        }
    }

    /// Adds one stored file. Entries past 65 535, or an archive past 4 GiB, are not supported
    /// (a feedback bundle is a dozen files and a few MiB); such an entry is skipped.
    pub fn add(&mut self, name: &str, data: &[u8]) {
        if !entry_fits(self.out.len(), self.central.len(), name.len(), data.len()) {
            return;
        }
        let (Ok(size), Ok(offset), Ok(name_len)) = (
            u32::try_from(data.len()),
            u32::try_from(self.out.len()),
            u16::try_from(name.len()),
        ) else {
            return;
        };
        if self.count == u16::MAX {
            return;
        }
        let crc = crc32(data);
        let out = &mut self.out;
        put32(out, LOCAL_SIG);
        put16(out, VERSION);
        put16(out, FLAG_UTF8);
        put16(out, 0); // stored
        put16(out, self.time);
        put16(out, self.date);
        put32(out, crc);
        put32(out, size);
        put32(out, size);
        put16(out, name_len);
        put16(out, 0); // extra
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);

        let c = &mut self.central;
        put32(c, CENTRAL_SIG);
        put16(c, VERSION); // made by
        put16(c, VERSION); // needed
        put16(c, FLAG_UTF8);
        put16(c, 0);
        put16(c, self.time);
        put16(c, self.date);
        put32(c, crc);
        put32(c, size);
        put32(c, size);
        put16(c, name_len);
        put16(c, 0); // extra
        put16(c, 0); // comment
        put16(c, 0); // disk
        put16(c, 0); // internal attributes
        put32(c, 0); // external attributes
        put32(c, offset);
        c.extend_from_slice(name.as_bytes());
        self.count += 1;
    }

    /// Appends the central directory and returns the archive bytes.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.out.len() as u32;
        let cd_size = self.central.len() as u32;
        self.out.extend_from_slice(&self.central);
        let out = &mut self.out;
        put32(out, END_SIG);
        put16(out, 0);
        put16(out, 0);
        put16(out, self.count);
        put16(out, self.count);
        put32(out, cd_size);
        put32(out, cd_offset);
        put16(out, 0); // comment
        self.out
    }
}

/// Whether an entry of `name_len` and `data_len` bytes can be added to an archive whose entries
/// so far take `out_len` bytes and whose central directory takes `central_len`: the whole archive
/// after this entry (its local header and data, every central record including its own, and the
/// end record) must stay addressable by the format's 32-bit offsets, so [`ZipWriter::finish`]
/// never wraps the central directory offset.
fn entry_fits(out_len: usize, central_len: usize, name_len: usize, data_len: usize) -> bool {
    const LOCAL_HEADER: u64 = 30;
    const CENTRAL_RECORD: u64 = 46;
    const END_RECORD: u64 = 22;
    if u16::try_from(name_len).is_err() {
        return false;
    }
    let name = name_len as u64;
    let end = (out_len as u64)
        .checked_add(LOCAL_HEADER + name)
        .and_then(|n| n.checked_add(data_len as u64))
        .and_then(|n| n.checked_add(central_len as u64))
        .and_then(|n| n.checked_add(CENTRAL_RECORD + name + END_RECORD));
    end.is_some_and(|end| end <= u64::from(u32::MAX))
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn get16(b: &[u8], at: usize) -> Result<u16, String> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| format!("truncated archive at byte {at}"))
}

fn get32(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| format!("truncated archive at byte {at}"))
}

/// One file read back from an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    /// Path inside the archive.
    pub name: String,
    /// The file's bytes (CRC verified).
    pub data: Vec<u8>,
}

/// The entry names of an archive, in directory order (no data copied).
pub fn entry_names(bytes: &[u8]) -> Result<Vec<String>, String> {
    Ok(walk(bytes, false)?.into_iter().map(|e| e.name).collect())
}

/// Reads every stored entry of an archive through its central directory, verifying each CRC.
/// Compressed entries (another tool's archive) are an error, and so are entry names no bundle
/// writes: absolute, with `..` or a backslash, or a duplicate. A bundle is a dropped-in file, so
/// its names are untrusted; even so, never join them to a disk path.
pub fn read_entries(bytes: &[u8]) -> Result<Vec<ZipEntry>, String> {
    walk(bytes, true)
}

fn walk(bytes: &[u8], with_data: bool) -> Result<Vec<ZipEntry>, String> {
    if bytes.len() < 22 {
        return Err("not a zip archive (too short)".into());
    }
    let lowest = bytes.len().saturating_sub(22 + 65_535);
    let mut end = None;
    let mut at = bytes.len() - 22;
    loop {
        if get32(bytes, at)? == END_SIG {
            end = Some(at);
            break;
        }
        if at == lowest {
            break;
        }
        at -= 1;
    }
    let end = end.ok_or("not a zip archive (no end-of-central-directory record)")?;
    let count = get16(bytes, end + 10)? as usize;
    let mut at = get32(bytes, end + 16)? as usize;
    let mut entries = Vec::with_capacity(count);
    let mut seen = std::collections::HashSet::with_capacity(count);
    // Each entry's data range in `bytes`: stored entries never share bytes, so the copies made
    // here can never add up to more than the archive itself (no small file decodes to gigabytes).
    let mut ranges: Vec<(usize, usize, usize)> = Vec::with_capacity(if with_data { count } else { 0 });
    for _ in 0..count {
        if get32(bytes, at)? != CENTRAL_SIG {
            return Err(format!("bad central directory entry at byte {at}"));
        }
        let method = get16(bytes, at + 10)?;
        let crc = get32(bytes, at + 16)?;
        let size = get32(bytes, at + 20)? as usize;
        let name_len = get16(bytes, at + 28)? as usize;
        let extra_len = get16(bytes, at + 30)? as usize;
        let comment_len = get16(bytes, at + 32)? as usize;
        let local = get32(bytes, at + 42)? as usize;
        let name = bytes
            .get(at + 46..at + 46 + name_len)
            .ok_or("truncated entry name")?;
        let name = String::from_utf8_lossy(name).into_owned();
        if !safe_entry_name(&name) || !seen.insert(name.clone()) {
            return Err(format!("entry name {name:?} is not allowed"));
        }
        at += 46 + name_len + extra_len + comment_len;
        let data = if with_data {
            if method != 0 {
                return Err(format!("{name}: compressed entries are not supported"));
            }
            if get32(bytes, local)? != LOCAL_SIG {
                return Err(format!("{name}: bad local header"));
            }
            let start = local + 30 + get16(bytes, local + 26)? as usize + get16(bytes, local + 28)? as usize;
            let span = bytes
                .get(start..start + size)
                .ok_or_else(|| format!("{name}: truncated data"))?;
            // The local header too: an entry whose header sits inside another's data overlaps.
            ranges.push((local, start + size, entries.len()));
            if let Some(other) = overlapping(&mut ranges) {
                return Err(format!("{name}: its data overlaps entry {other}"));
            }
            let data = span.to_vec();
            if crc32(&data) != crc {
                return Err(format!("{name}: CRC mismatch"));
            }
            data
        } else {
            Vec::new()
        };
        entries.push(ZipEntry { name, data });
    }
    Ok(entries)
}

/// After pushing a range onto `ranges` (kept sorted by start, except the last), the index of an
/// earlier entry whose range overlaps the new one, if any. Insertion keeps the list sorted, so
/// each check is a binary search and two neighbours.
fn overlapping(ranges: &mut Vec<(usize, usize, usize)>) -> Option<usize> {
    let new = ranges.pop()?;
    let at = ranges.partition_point(|r| r.0 < new.0);
    let before = at.checked_sub(1).and_then(|i| ranges.get(i));
    if let Some(prev) = before
        && prev.1 > new.0
    {
        return Some(prev.2);
    }
    if let Some(next) = ranges.get(at)
        && next.0 < new.1
    {
        return Some(next.2);
    }
    ranges.insert(at, new);
    None
}

/// Whether `name` is a relative `/`-separated path that stays inside the archive.
fn safe_entry_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains(['\\', ':'])
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_that_could_leave_a_folder_are_refused() {
        for name in ["../evil.sh", "/etc/passwd", "a/../../b", r"a\b", "C:/x", ""] {
            assert!(!safe_entry_name(name), "{name:?}");
        }
        assert!(safe_entry_name("log_tail.txt"));
        assert!(safe_entry_name("frames/001.txt"));
    }

    #[test]
    fn crc32_matches_the_check_value() {
        // The standard CRC-32/ISO-HDLC check value.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn round_trips_entries() {
        let mut zip = ZipWriter::new(1_791_000_000);
        zip.add("note.txt", "hello — wörld".as_bytes());
        zip.add("empty.txt", b"");
        zip.add("dir/data.bin", &[0u8, 1, 2, 255]);
        let bytes = zip.finish();
        let entries = read_entries(&bytes).expect("reads");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "note.txt");
        assert_eq!(entries[0].data, "hello — wörld".as_bytes());
        assert_eq!(entries[1].data, b"");
        assert_eq!(entries[2].name, "dir/data.bin");
        assert_eq!(entries[2].data, vec![0u8, 1, 2, 255]);
        assert_eq!(
            entry_names(&bytes).expect("names"),
            vec!["note.txt", "empty.txt", "dir/data.bin"]
        );
    }

    #[test]
    fn rejects_garbage_and_corruption() {
        assert!(read_entries(b"not a zip").is_err());
        let mut zip = ZipWriter::new(0);
        zip.add("a.txt", b"abc");
        let mut bytes = zip.finish();
        // Flip a data byte: the CRC check catches it.
        let at = 30 + "a.txt".len();
        bytes[at] ^= 0xFF;
        assert!(read_entries(&bytes).expect_err("corrupt").contains("CRC"));
    }

    /// An archive whose `names` central entries all point at one stored `data` at offset 0.
    fn aliased_archive(data: &[u8], names: &[String]) -> Vec<u8> {
        let crc = crc32(data);
        let mut out = Vec::new();
        put32(&mut out, LOCAL_SIG);
        put16(&mut out, VERSION);
        put16(&mut out, FLAG_UTF8);
        put16(&mut out, 0);
        put32(&mut out, 0); // time, date
        put32(&mut out, crc);
        put32(&mut out, data.len() as u32);
        put32(&mut out, data.len() as u32);
        put16(&mut out, 1);
        put16(&mut out, 0);
        out.push(b'x');
        out.extend_from_slice(data);
        let cd_offset = out.len() as u32;
        let mut central = Vec::new();
        for name in names {
            put32(&mut central, CENTRAL_SIG);
            put16(&mut central, VERSION);
            put16(&mut central, VERSION);
            put16(&mut central, FLAG_UTF8);
            put16(&mut central, 0);
            put32(&mut central, 0); // time, date
            put32(&mut central, crc);
            put32(&mut central, data.len() as u32);
            put32(&mut central, data.len() as u32);
            put16(&mut central, name.len() as u16);
            put32(&mut central, 0); // extra, comment
            put32(&mut central, 0); // disk, internal
            put32(&mut central, 0); // external
            put32(&mut central, 0); // local header offset
            central.extend_from_slice(name.as_bytes());
        }
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        put32(&mut out, END_SIG);
        put32(&mut out, 0);
        put16(&mut out, names.len() as u16);
        put16(&mut out, names.len() as u16);
        put32(&mut out, cd_size);
        put32(&mut out, cd_offset);
        put16(&mut out, 0);
        out
    }

    /// audit-v034b-formats M1: many central entries aliasing one stored blob made a small
    /// archive decode into an unbounded number of copies (an allocation bomb).
    #[test]
    fn entries_that_share_one_data_range_are_refused() {
        let data = vec![7u8; 4096];
        let names: Vec<String> = (0..300).map(|i| format!("e{i:04}")).collect();
        let bytes = aliased_archive(&data, &names);
        let err = read_entries(&bytes).expect_err("aliased entries");
        assert!(err.contains("overlap") || err.contains("larger"), "{err}");
        // One entry over the same layout is fine.
        let single = aliased_archive(&data, &names[..1]);
        assert_eq!(read_entries(&single).expect("one entry")[0].data, data);
    }

    /// audit-v034b-formats L8: `add` checked only where an entry starts, so an entry that ends
    /// past 4 GiB was written and `finish` wrapped the central directory offset.
    #[test]
    fn an_entry_that_would_push_the_archive_past_four_gib_does_not_fit() {
        assert!(entry_fits(0, 0, 8, 1024));
        let max = u32::MAX as usize;
        assert!(
            !entry_fits(max - 100, 0, 8, 1024),
            "starts below 4 GiB, ends above"
        );
        assert!(
            !entry_fits(max - 2000, 1000, 8, 900),
            "the central directory no longer fits"
        );
        assert!(entry_fits(max - 4000, 0, 8, 1000));
    }

    #[test]
    fn dos_time_encodes_civil_fields() {
        // 2026-10-09 12:34:56 UTC.
        let (time, date) = dos_time(1_791_549_296);
        assert_eq!(time >> 11, 12);
        assert_eq!((time >> 5) & 0x3F, 34);
        assert_eq!((time & 0x1F) * 2, 56);
        assert_eq!((date >> 9) + 1980, 2026);
        assert_eq!((date >> 5) & 0x0F, 10);
        assert_eq!(date & 0x1F, 9);
    }
}
