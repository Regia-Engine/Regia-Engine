// SPDX-License-Identifier: MPL-2.0
//! `dist_archive`: archive writers for `cargo xtask dist`: a stored (uncompressed) zip for
//! Windows bundles and a POSIX ustar tar for Steam Deck bundles. Both are written in-process, so
//! the bundle needs no `zip`, `tar` or PowerShell on the build machine, and both are
//! deterministic: the same files and the same `mtime` give the same bytes.
//!
//! - zip: the stored zip writer and reader of feedback bundles (`regia_framework::feedback::zip`,
//!   PKWARE `APPNOTE.TXT`, no Zip64). That writer skips an entry it cannot store (a file over
//!   4 GiB, past 65 535 entries or past 4 GiB in all), so [`write_zip`] reads the archive back and
//!   fails unless every file is in it. Zip members carry no Unix mode; Windows has none.
//! - tar: written here from the ustar header of POSIX.1-1988 / IEEE Std 1003.1 (`pax` "ustar
//!   Interchange Format"; provenance: `docs/provenance.md`): 512-byte blocks, octal fields, the
//!   checksum over the header with the checksum field as spaces, a 155-byte name prefix, two zero
//!   blocks at the end. Mode bits are kept, so the Deck binary stays executable.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use regia_framework::feedback::zip::{ZipWriter, entry_names, read_entries};

/// What one archive entry holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntrySource {
    /// A directory (no data).
    Dir,
    /// The bytes of a file on disk.
    File(PathBuf),
    /// Bytes held in memory.
    Bytes(Vec<u8>),
}

/// One archive entry: a `/`-separated relative name, its contents and its Unix mode bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Relative name inside the archive, `/`-separated, no leading `/`, no `..`. Directories do
    /// not end in `/` here; the tar writer adds it.
    pub name: String,
    /// What the entry holds.
    pub source: EntrySource,
    /// Unix permission bits (`0o755` for executables and directories, `0o644` otherwise).
    pub mode: u32,
}

impl Entry {
    /// A directory entry with mode `0o755`.
    #[must_use]
    pub fn dir(name: &str) -> Self {
        Self {
            name: name.to_string(),
            source: EntrySource::Dir,
            mode: 0o755,
        }
    }

    /// A file entry read from `path`.
    #[must_use]
    pub fn file(name: &str, path: &Path, executable: bool) -> Self {
        Self {
            name: name.to_string(),
            source: EntrySource::File(path.to_path_buf()),
            mode: if executable { 0o755 } else { 0o644 },
        }
    }
}

/// Checks an entry name: relative, `/`-separated, no empty, `.` or `..` component, no `\`.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        bail!("archive entry name {name:?} must be a relative `/`-separated path without `.`/`..`");
    }
    Ok(())
}

/// The bytes of a file or in-memory entry, streamed through `f` in 64 KiB chunks. Returns the
/// number of bytes.
fn for_each_chunk(source: &EntrySource, mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<u64> {
    match source {
        EntrySource::Dir => Ok(0),
        EntrySource::Bytes(bytes) => {
            f(bytes)?;
            Ok(bytes.len() as u64)
        }
        EntrySource::File(path) => {
            let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            loop {
                let n = file
                    .read(&mut buf)
                    .with_context(|| format!("reading {}", path.display()))?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                f(&buf[..n])?;
            }
            Ok(total)
        }
    }
}

/// The size of an entry's data.
fn source_len(source: &EntrySource) -> Result<u64> {
    match source {
        EntrySource::Dir => Ok(0),
        EntrySource::Bytes(bytes) => Ok(bytes.len() as u64),
        EntrySource::File(path) => Ok(std::fs::metadata(path)
            .with_context(|| format!("reading the size of {}", path.display()))?
            .len()),
    }
}

/// `<out>.tmp`.
fn tmp_path(out: &Path) -> PathBuf {
    let mut name = out.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    out.with_file_name(name)
}

/// Writes the files of `entries` as a stored zip to `out` (through `<out>.tmp` and a rename),
/// each with the modification time `mtime` (Unix seconds). Directory entries are left out (zip
/// tools create the folders from the file names). The archive is built in memory.
///
/// # Errors
/// An invalid name, a file the zip cannot hold (see the module docs), or IO.
pub fn write_zip(out: &Path, entries: &[Entry], mtime: u64) -> Result<()> {
    let mut zip = ZipWriter::new(mtime);
    let mut files = 0usize;
    for entry in entries {
        check_name(&entry.name)?;
        let owned;
        let data: &[u8] = match &entry.source {
            EntrySource::Dir => continue,
            EntrySource::Bytes(bytes) => bytes,
            EntrySource::File(path) => {
                owned = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
                &owned
            }
        };
        zip.add(&entry.name, data);
        files += 1;
    }
    let bytes = zip.finish();
    let stored = entry_names(&bytes)
        .map_err(|e| anyhow::anyhow!("reading back the zip for {}: {e}", out.display()))?
        .len();
    if stored != files {
        bail!(
            "only {stored} of {files} files fit in {} (a zip without Zip64 holds files under 4 GiB, \
             at most 65 535 of them and 4 GiB in all)",
            out.display()
        );
    }
    let tmp = tmp_path(out);
    std::fs::write(&tmp, &bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, out).with_context(|| format!("renaming {} to {}", tmp.display(), out.display()))
}

/// Writes `value` as a NUL-terminated octal field of `field.len()` bytes.
fn octal(field: &mut [u8], value: u64) -> Result<()> {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    if text.len() > digits {
        bail!("{value} does not fit a {digits}-digit ustar octal field");
    }
    field[..digits].copy_from_slice(text.as_bytes());
    field[digits] = 0;
    Ok(())
}

/// Splits a ustar name into (prefix, name): the whole name when it fits 100 bytes, else at a `/`
/// so that the prefix fits 155 bytes and the rest 100.
fn split_ustar_name(name: &str) -> Result<(&str, &str)> {
    if name.len() <= 100 {
        return Ok(("", name));
    }
    for (i, _) in name.match_indices('/') {
        let (prefix, rest) = (&name[..i], &name[i + 1..]);
        if prefix.len() <= 155 && rest.len() <= 100 && !rest.is_empty() {
            return Ok((prefix, rest));
        }
    }
    bail!("{name:?} is too long for a ustar header (155-byte prefix + 100-byte name)")
}

/// One 512-byte ustar header.
fn ustar_header(name: &str, is_dir: bool, mode: u32, size: u64, mtime: u64) -> Result<[u8; 512]> {
    let full = if is_dir {
        format!("{name}/")
    } else {
        name.to_string()
    };
    let (prefix, base) = split_ustar_name(&full)?;
    let mut h = [0u8; 512];
    h[..base.len()].copy_from_slice(base.as_bytes());
    octal(&mut h[100..108], u64::from(mode & 0o7777))?;
    octal(&mut h[108..116], 0)?; // uid
    octal(&mut h[116..124], 0)?; // gid
    octal(&mut h[124..136], size)?;
    octal(&mut h[136..148], mtime)?;
    h[156] = if is_dir { b'5' } else { b'0' };
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[345..345 + prefix.len()].copy_from_slice(prefix.as_bytes());
    // The checksum is the byte sum of the header with the checksum field as eight spaces, written
    // as six octal digits, a NUL and a space.
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    let text = format!("{sum:06o}");
    h[148..154].copy_from_slice(text.as_bytes());
    h[154] = 0;
    h[155] = b' ';
    Ok(h)
}

/// Writes `entries` as an uncompressed ustar archive to `out` (through `<out>.tmp` and a rename),
/// owner and group 0, every entry with the same `mtime`. GNU tar, bsdtar and Python's `tarfile`
/// read it; SteamOS has GNU tar.
///
/// # Errors
/// An invalid or too long name, a file of 8 GiB or more, or IO.
pub fn write_tar(out: &Path, entries: &[Entry], mtime: u64) -> Result<()> {
    for entry in entries {
        check_name(&entry.name)?;
    }
    let tmp = tmp_path(out);
    let mut w = BufWriter::new(File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?);
    for entry in entries {
        let is_dir = entry.source == EntrySource::Dir;
        let size = source_len(&entry.source)?;
        w.write_all(&ustar_header(&entry.name, is_dir, entry.mode, size, mtime)?)?;
        let written = for_each_chunk(&entry.source, |chunk| {
            w.write_all(chunk)?;
            Ok(())
        })?;
        if written != size {
            bail!("{} changed while it was being archived", entry.name);
        }
        let pad = (size.next_multiple_of(512) - size) as usize;
        w.write_all(&[0u8; 512][..pad])?;
    }
    w.write_all(&[0u8; 1024])?;
    let file = w
        .into_inner()
        .map_err(|e| anyhow::anyhow!("flushing {}: {}", tmp.display(), e.error()))?;
    file.sync_all()
        .with_context(|| format!("syncing {}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, out).with_context(|| format!("renaming {} to {}", tmp.display(), out.display()))
}

/// One member of an archive as [`list_zip`] / [`list_tar`] read it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Name as stored (tar directories end in `/`).
    pub name: String,
    /// Unix permission bits (tar); `None` for zip members.
    pub mode: Option<u32>,
    /// The member's bytes (empty for directories).
    pub data: Vec<u8>,
}

/// Reads a stored zip back (CRC-checked by the feedback zip reader). Used by `dist --verify` and
/// the tests.
///
/// # Errors
/// A malformed or compressed archive, or a CRC mismatch.
pub fn list_zip(bytes: &[u8]) -> Result<Vec<Member>> {
    let entries = read_entries(bytes).map_err(|e| anyhow::anyhow!("reading the zip: {e}"))?;
    Ok(entries
        .into_iter()
        .map(|e| Member {
            name: e.name,
            mode: None,
            data: e.data,
        })
        .collect())
}

/// Parses a NUL- or space-terminated octal field.
fn parse_octal(field: &[u8]) -> Result<u64> {
    let text: String = field
        .iter()
        .take_while(|&&b| b != 0 && b != b' ')
        .map(|&b| b as char)
        .collect();
    u64::from_str_radix(text.trim_start(), 8).with_context(|| format!("bad ustar octal field {text:?}"))
}

/// Reads a ustar archive back, checking every header checksum. Used by `dist --verify` and the
/// tests; it reads what [`write_tar`] writes (regular files and directories).
///
/// # Errors
/// A malformed header, a checksum mismatch or truncated data.
pub fn list_tar(bytes: &[u8]) -> Result<Vec<Member>> {
    let mut members = Vec::new();
    let mut at = 0usize;
    while at + 512 <= bytes.len() {
        let h = &bytes[at..at + 512];
        if h.iter().all(|&b| b == 0) {
            return Ok(members);
        }
        let stored = parse_octal(&h[148..156])?;
        let sum: u64 = h
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    u64::from(b' ')
                } else {
                    u64::from(b)
                }
            })
            .sum();
        if sum != stored {
            bail!("ustar header at {at}: checksum {stored} != {sum}");
        }
        if h[257..263] != *b"ustar\0" {
            bail!("ustar header at {at}: no ustar magic");
        }
        let field = |r: std::ops::Range<usize>| -> String {
            h[r].iter().take_while(|&&b| b != 0).map(|&b| b as char).collect()
        };
        let base = field(0..100);
        let prefix = field(345..500);
        let name = if prefix.is_empty() {
            base
        } else {
            format!("{prefix}/{base}")
        };
        let mode = parse_octal(&h[100..108])? as u32;
        let size = usize::try_from(parse_octal(&h[124..136])?).context("ustar size out of range")?;
        let data_end = (at + 512).checked_add(size).context("ustar size out of range")?;
        let data = bytes
            .get(at + 512..data_end)
            .context("truncated ustar data")?
            .to_vec();
        members.push(Member {
            name,
            mode: Some(mode),
            data,
        });
        at = data_end.next_multiple_of(512);
    }
    bail!("ustar archive has no end-of-archive blocks")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_with_dot_dot_or_backslash_are_refused() {
        for bad in ["", "/abs", "a/../b", "a\\b", "a//b", "./a"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_name("regia-deck-0.1.27/vfx_lab").is_ok());
    }

    #[test]
    fn long_names_split_into_the_ustar_prefix() -> Result<()> {
        let long = format!("{}/{}", "d".repeat(120), "f".repeat(90));
        let (prefix, name) = split_ustar_name(&long)?;
        assert_eq!(prefix.len(), 120);
        assert_eq!(name.len(), 90);
        assert!(split_ustar_name(&"x".repeat(101)).is_err());
        Ok(())
    }

    #[test]
    fn ustar_header_checksum_and_fields() -> Result<()> {
        let h = ustar_header("bundle/vfx_lab", false, 0o755, 1234, 1_792_611_413)?;
        assert_eq!(&h[..14], b"bundle/vfx_lab");
        assert_eq!(parse_octal(&h[100..108])?, 0o755);
        assert_eq!(parse_octal(&h[124..136])?, 1234);
        assert_eq!(parse_octal(&h[136..148])?, 1_792_611_413);
        assert_eq!(h[156], b'0');
        let sum: u64 = h
            .iter()
            .enumerate()
            .map(|(i, &b)| if (148..156).contains(&i) { 32 } else { u64::from(b) })
            .sum();
        assert_eq!(parse_octal(&h[148..156])?, sum);
        Ok(())
    }

    #[test]
    fn in_memory_entries_round_trip_through_both_archives() -> Result<()> {
        let dir = regia_test_utils::unique_temp_dir("dist-archive");
        let entries = [
            Entry::dir("top"),
            Entry {
                name: "top/run".to_string(),
                source: EntrySource::Bytes(vec![7u8; 1000]),
                mode: 0o755,
            },
            Entry {
                name: "top/empty.txt".to_string(),
                source: EntrySource::Bytes(Vec::new()),
                mode: 0o644,
            },
        ];
        let tar = dir.join("a.tar");
        write_tar(&tar, &entries, 0)?;
        let members = list_tar(&std::fs::read(&tar)?)?;
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["top/", "top/run", "top/empty.txt"]);
        assert_eq!(members[1].data, [7u8; 1000]);
        assert_eq!(members[1].mode, Some(0o755));
        assert!(members[2].data.is_empty());
        // 3 headers + 2 data blocks (1000 bytes) + the 2 end blocks.
        assert_eq!(std::fs::metadata(&tar)?.len(), 512 * 7);

        let zip = dir.join("a.zip");
        write_zip(&zip, &entries, 0)?;
        let members = list_zip(&std::fs::read(&zip)?)?;
        let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["top/run", "top/empty.txt"], "directories are left out");
        assert_eq!(members[0].data, [7u8; 1000]);
        assert_eq!(members[0].mode, None);
        assert!(!dir.join("a.zip.tmp").exists() && !dir.join("a.tar.tmp").exists());

        assert!(write_tar(&tar, &[Entry::dir("../up")], 0).is_err());
        assert!(
            !dir.join("a.tar.tmp").exists(),
            "a bad name is refused before the temp file"
        );
        Ok(())
    }
}
