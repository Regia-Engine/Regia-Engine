// SPDX-License-Identifier: MPL-2.0
//! The symbol versions an ELF binary needs from its shared libraries (`GLIBC_2.31` from
//! `libc.so.6`, ...), read from the `SHT_GNU_verneed` section (`.gnu.version_r`) as bytes, so the
//! glibc floor check of `regia package` needs no binutils on the build machine or in a build
//! container (C-26, ADR-8835).
//!
//! This answers the question `objdump -T <binary> | grep GLIBC_` answers, from the version-needed
//! table the dynamic loader itself checks: the newest `GLIBC_x.y` there is the oldest glibc the
//! binary starts on. Layouts follow the public System V gABI (section headers) and the Linux
//! Standard Base Core specification, "Symbol Versioning" (`Elf64_Verneed`, `Elf64_Vernaux`); see
//! `docs/provenance.md`.
//!
//! Supported: ELF64 little-endian files with section headers (every x86_64 and aarch64 Linux
//! binary a package ships). Every offset and count is checked against the slice with checked
//! arithmetic and every loop is bounded by the bytes that exist, so a damaged file is an error,
//! never a panic.

use crate::commands::ship_check::GlibcVersion;

/// `SHT_GNU_verneed`: the version-needed section type (LSB Core, "Symbol Versioning").
const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;
/// Size of an ELF64 section header.
const SHDR_SIZE: usize = 64;
/// Size of an `Elf64_Verneed` and of an `Elf64_Vernaux` entry.
const ENTRY_SIZE: usize = 16;

/// One version a binary needs: the library (`vn_file`) and the version name (`vna_name`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionNeed {
    /// Library soname the version is needed from (`libc.so.6`).
    pub file: String,
    /// Version name (`GLIBC_2.31`).
    pub version: String,
}

/// Why the version table could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ElfVersionError {
    /// The bytes are not an ELF file.
    #[error("elf_not_elf: the file is not an ELF binary")]
    NotElf,
    /// An ELF file of a shape this reader does not follow (32-bit, big-endian, no section headers).
    #[error("elf_unsupported: {0}")]
    Unsupported(&'static str),
    /// A table, offset or count points outside the file.
    #[error("elf_malformed: {0}")]
    Malformed(&'static str),
}

fn slice(bytes: &[u8], at: usize, len: usize) -> Result<&[u8], ElfVersionError> {
    at.checked_add(len)
        .and_then(|end| bytes.get(at..end))
        .ok_or(ElfVersionError::Malformed("a table points outside the file"))
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, ElfVersionError> {
    let b = slice(bytes, at, 2)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, ElfVersionError> {
    let b = slice(bytes, at, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, ElfVersionError> {
    let b = slice(bytes, at, 8)?;
    let mut v = [0u8; 8];
    v.copy_from_slice(b);
    Ok(u64::from_le_bytes(v))
}

fn to_usize(v: u64) -> Result<usize, ElfVersionError> {
    usize::try_from(v).map_err(|_| ElfVersionError::Malformed("an offset does not fit in memory"))
}

/// One section header's fields this reader uses.
struct Section {
    kind: u32,
    offset: usize,
    size: usize,
    link: u32,
    info: u32,
}

fn section(bytes: &[u8], shoff: usize, shentsize: usize, index: usize) -> Result<Section, ElfVersionError> {
    let at = index
        .checked_mul(shentsize)
        .and_then(|o| o.checked_add(shoff))
        .ok_or(ElfVersionError::Malformed("section header offset overflows"))?;
    slice(bytes, at, SHDR_SIZE)?;
    Ok(Section {
        kind: u32_at(bytes, at + 4)?,
        offset: to_usize(u64_at(bytes, at + 24)?)?,
        size: to_usize(u64_at(bytes, at + 32)?)?,
        link: u32_at(bytes, at + 40)?,
        info: u32_at(bytes, at + 44)?,
    })
}

/// The NUL-terminated string at `at` inside the string table section `table`.
fn string(bytes: &[u8], table: &Section, at: u32) -> Result<String, ElfVersionError> {
    let at = usize::try_from(at).map_err(|_| ElfVersionError::Malformed("string offset"))?;
    if at >= table.size {
        return Err(ElfVersionError::Malformed(
            "a name points outside its string table",
        ));
    }
    let data = slice(bytes, table.offset, table.size)?;
    let tail = &data[at..];
    let end = tail
        .iter()
        .position(|&b| b == 0)
        .ok_or(ElfVersionError::Malformed("a name is not NUL-terminated"))?;
    Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
}

/// Every version the binary needs, in table order. A static binary (no `.gnu.version_r`) needs
/// none.
///
/// # Errors
///
/// [`ElfVersionError::NotElf`] for other files, [`ElfVersionError::Unsupported`] for 32-bit or
/// big-endian ELF or a file without section headers, [`ElfVersionError::Malformed`] when a table
/// points outside the file.
pub fn version_needs(bytes: &[u8]) -> Result<Vec<VersionNeed>, ElfVersionError> {
    if !bytes.starts_with(&[0x7f, b'E', b'L', b'F']) {
        return Err(ElfVersionError::NotElf);
    }
    match (bytes.get(4), bytes.get(5)) {
        (Some(2), Some(1)) => {}
        (Some(2), Some(_)) => return Err(ElfVersionError::Unsupported("big-endian ELF")),
        (Some(_), _) => return Err(ElfVersionError::Unsupported("32-bit ELF")),
        _ => return Err(ElfVersionError::Malformed("truncated ELF header")),
    }
    let shoff = to_usize(u64_at(bytes, 0x28)?)?;
    let shentsize = usize::from(u16_at(bytes, 0x3a)?);
    let mut shnum = usize::from(u16_at(bytes, 0x3c)?);
    if shoff == 0 {
        return Err(ElfVersionError::Unsupported("the file has no section headers"));
    }
    if shentsize < SHDR_SIZE {
        return Err(ElfVersionError::Malformed("section header entry too small"));
    }
    if shnum == 0 {
        // Extended numbering (gABI): the real count is section 0's `sh_size`.
        shnum = section(bytes, shoff, shentsize, 0)?.size;
    }
    // Bound the walk by the headers that fit in the file, whatever the count claims.
    let fit = bytes.len().saturating_sub(shoff) / shentsize;
    if shnum > fit {
        return Err(ElfVersionError::Malformed(
            "more section headers than the file holds",
        ));
    }
    let mut needs = Vec::new();
    for index in 0..shnum {
        let verneed = section(bytes, shoff, shentsize, index)?;
        if verneed.kind != SHT_GNU_VERNEED {
            continue;
        }
        let link = usize::try_from(verneed.link).map_err(|_| ElfVersionError::Malformed("sh_link"))?;
        if link >= shnum {
            return Err(ElfVersionError::Malformed(
                "the version table links no string table",
            ));
        }
        let strings = section(bytes, shoff, shentsize, link)?;
        let data = slice(bytes, verneed.offset, verneed.size)?;
        // Every entry is at least 16 bytes: no table holds more entries than that.
        let budget = data.len() / ENTRY_SIZE;
        let mut seen = 0usize;
        let mut at = 0usize;
        for _ in 0..verneed.info {
            seen += 1;
            if seen > budget {
                return Err(ElfVersionError::Malformed(
                    "more version entries than the table holds",
                ));
            }
            slice(data, at, ENTRY_SIZE)?;
            let count = u16_at(data, at + 2)?;
            let file = string(bytes, &strings, u32_at(data, at + 4)?)?;
            let aux = to_usize(u64::from(u32_at(data, at + 8)?))?;
            let next = to_usize(u64::from(u32_at(data, at + 12)?))?;
            let mut aux_at = at
                .checked_add(aux)
                .ok_or(ElfVersionError::Malformed("vn_aux overflows"))?;
            for _ in 0..count {
                seen += 1;
                if seen > budget {
                    return Err(ElfVersionError::Malformed(
                        "more version entries than the table holds",
                    ));
                }
                slice(data, aux_at, ENTRY_SIZE)?;
                let version = string(bytes, &strings, u32_at(data, aux_at + 8)?)?;
                needs.push(VersionNeed {
                    file: file.clone(),
                    version,
                });
                let aux_next = to_usize(u64::from(u32_at(data, aux_at + 12)?))?;
                if aux_next == 0 {
                    break;
                }
                aux_at = aux_at
                    .checked_add(aux_next)
                    .ok_or(ElfVersionError::Malformed("vna_next overflows"))?;
            }
            if next == 0 {
                break;
            }
            at = at
                .checked_add(next)
                .ok_or(ElfVersionError::Malformed("vn_next overflows"))?;
        }
    }
    Ok(needs)
}

/// The newest `GLIBC_x.y` version in `needs` with the entry it came from (`GLIBC_PRIVATE` and other
/// names are ignored; on a tie the first entry wins).
#[must_use]
pub fn newest_glibc_need(needs: &[VersionNeed]) -> Option<(GlibcVersion, &VersionNeed)> {
    let mut newest: Option<(GlibcVersion, &VersionNeed)> = None;
    for need in needs {
        let Some(version) = need.version.strip_prefix("GLIBC_").and_then(GlibcVersion::parse) else {
            continue;
        };
        if newest.is_none_or(|(best, _)| version > best) {
            newest = Some((version, need));
        }
    }
    newest
}

/// The newest `GLIBC_x.y` version an ELF binary needs: the oldest glibc it starts on (`None`: it
/// needs no versioned glibc symbol, a static binary). `regia package` reads the same through
/// `ship_check::linux_binary_report`.
///
/// # Errors
///
/// As [`version_needs`].
#[cfg(test)]
pub fn max_glibc_needed(bytes: &[u8]) -> Result<Option<GlibcVersion>, ElfVersionError> {
    Ok(newest_glibc_need(&version_needs(bytes)?).map(|(version, _)| version))
}

/// Test fixtures: a minimal ELF64 file with a `.dynstr` and a `.gnu.version_r` section.
#[cfg(test)]
pub(crate) mod fixture {
    /// An ELF64 little-endian file whose version-needed table lists, per `(file, versions)`, the
    /// versions needed from that library. Only the section headers the reader walks are present
    /// (null, `.dynstr`, `.gnu.version_r`).
    pub(crate) fn elf_with_needs(needs: &[(&str, &[&str])]) -> Vec<u8> {
        fn name_at(s: &str, strtab: &mut Vec<u8>) -> u32 {
            let at = u32::try_from(strtab.len()).unwrap_or(u32::MAX);
            strtab.extend_from_slice(s.as_bytes());
            strtab.push(0);
            at
        }
        let mut strtab = vec![0u8];
        let mut verneed = Vec::new();
        for (i, (file, versions)) in needs.iter().enumerate() {
            let file_at = name_at(file, &mut strtab);
            let count = u16::try_from(versions.len()).unwrap_or(u16::MAX);
            let last = i + 1 == needs.len();
            let size = 16 + 16 * versions.len();
            verneed.extend_from_slice(&1u16.to_le_bytes()); // vn_version
            verneed.extend_from_slice(&count.to_le_bytes()); // vn_cnt
            verneed.extend_from_slice(&file_at.to_le_bytes()); // vn_file
            verneed.extend_from_slice(&16u32.to_le_bytes()); // vn_aux
            let next = if last { 0 } else { u32::try_from(size).unwrap_or(0) };
            verneed.extend_from_slice(&next.to_le_bytes()); // vn_next
            for (j, version) in versions.iter().enumerate() {
                let version_at = name_at(version, &mut strtab);
                verneed.extend_from_slice(&0u32.to_le_bytes()); // vna_hash (unused here)
                verneed.extend_from_slice(&0u16.to_le_bytes()); // vna_flags
                verneed.extend_from_slice(&u16::try_from(j + 2).unwrap_or(0).to_le_bytes()); // vna_other
                verneed.extend_from_slice(&version_at.to_le_bytes()); // vna_name
                let aux_next: u32 = if j + 1 == versions.len() { 0 } else { 16 };
                verneed.extend_from_slice(&aux_next.to_le_bytes()); // vna_next
            }
        }
        let header = 64usize;
        let strtab_off = header;
        let verneed_off = strtab_off + strtab.len();
        let shoff = (verneed_off + verneed.len()).next_multiple_of(8);
        let mut out = vec![0u8; header];
        out[..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        out[4] = 2; // ELFCLASS64
        out[5] = 1; // ELFDATA2LSB
        out[6] = 1; // EV_CURRENT
        out[0x10..0x12].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        out[0x12..0x14].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64
        out[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        out[0x34..0x36].copy_from_slice(&64u16.to_le_bytes()); // e_ehsize
        out[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes()); // e_shentsize
        out[0x3c..0x3e].copy_from_slice(&3u16.to_le_bytes()); // e_shnum
        out.extend_from_slice(&strtab);
        out.extend_from_slice(&verneed);
        out.resize(shoff, 0);
        let mut shdr = |kind: u32, offset: usize, size: usize, link: u32, info: u32| {
            let mut h = [0u8; 64];
            h[4..8].copy_from_slice(&kind.to_le_bytes());
            h[24..32].copy_from_slice(&(offset as u64).to_le_bytes());
            h[32..40].copy_from_slice(&(size as u64).to_le_bytes());
            h[40..44].copy_from_slice(&link.to_le_bytes());
            h[44..48].copy_from_slice(&info.to_le_bytes());
            out.extend_from_slice(&h);
        };
        shdr(0, 0, 0, 0, 0);
        shdr(3, strtab_off, strtab.len(), 0, 0); // SHT_STRTAB
        let count = u32::try_from(needs.len()).unwrap_or(0);
        shdr(super::SHT_GNU_VERNEED, verneed_off, verneed.len(), 1, count);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::elf_with_needs;
    use super::*;

    /// The `objdump -T`-style check: the newest `GLIBC_` version needed across every library.
    #[test]
    fn max_glibc_needed_reads_the_fixture() {
        let elf = elf_with_needs(&[
            ("libm.so.6", &["GLIBC_2.2.5", "GLIBC_2.29"]),
            (
                "libc.so.6",
                &["GLIBC_2.2.5", "GLIBC_2.31", "GLIBC_PRIVATE", "GLIBC_2.17"],
            ),
            ("libgcc_s.so.1", &["GCC_3.0", "GCC_4.2.0"]),
        ]);
        let needs = version_needs(&elf).expect("fixture parses");
        assert_eq!(needs.len(), 8);
        assert_eq!(
            needs[0],
            VersionNeed {
                file: "libm.so.6".into(),
                version: "GLIBC_2.2.5".into()
            }
        );
        assert_eq!(needs[3].file, "libc.so.6");
        assert_eq!(
            max_glibc_needed(&elf).expect("parses"),
            GlibcVersion::parse("2.31")
        );
        let (newest, from) = newest_glibc_need(&needs).expect("glibc versions");
        assert_eq!(newest, GlibcVersion { major: 2, minor: 31 });
        assert_eq!(from.file, "libc.so.6", "the library the floor comes from");
        // A newer libc symbol moves the floor; libgcc's versions never count.
        let newer = elf_with_needs(&[("libc.so.6", &["GLIBC_2.34"]), ("libgcc_s.so.1", &["GCC_7.0.0"])]);
        assert_eq!(
            max_glibc_needed(&newer).expect("parses"),
            GlibcVersion::parse("2.34")
        );
    }

    #[test]
    fn a_binary_without_version_needs_has_no_glibc_floor() {
        let elf = elf_with_needs(&[]);
        assert_eq!(version_needs(&elf).expect("parses"), Vec::new());
        assert_eq!(max_glibc_needed(&elf).expect("parses"), None);
    }

    #[test]
    fn damaged_and_foreign_files_are_errors_not_panics() {
        assert_eq!(version_needs(b"MZ\x90\0"), Err(ElfVersionError::NotElf));
        let mut elf32 = elf_with_needs(&[("libc.so.6", &["GLIBC_2.31"])]);
        elf32[4] = 1;
        assert!(matches!(
            version_needs(&elf32),
            Err(ElfVersionError::Unsupported(_))
        ));
        let mut big = elf_with_needs(&[("libc.so.6", &["GLIBC_2.31"])]);
        big[5] = 2;
        assert!(matches!(
            version_needs(&big),
            Err(ElfVersionError::Unsupported(_))
        ));
        // Every truncation of a valid file is an error (or, before the table, still valid).
        let elf = elf_with_needs(&[("libc.so.6", &["GLIBC_2.31", "GLIBC_2.17"])]);
        for len in 0..elf.len() {
            let _ = version_needs(&elf[..len]);
        }
        // A claimed entry count far beyond the table is refused, not walked.
        let mut huge = elf.clone();
        let shoff =
            usize::try_from(u64::from_le_bytes(huge[0x28..0x30].try_into().expect("8"))).expect("fits");
        let info_at = shoff + 2 * 64 + 44;
        huge[info_at..info_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        // The one real entry ends the chain (vn_next 0), so the count alone cannot loop.
        assert!(version_needs(&huge).is_ok());
        // A name offset outside the string table.
        let mut bad_name = elf.clone();
        let verneed_off = usize::try_from(u64::from_le_bytes(
            bad_name[shoff + 2 * 64 + 24..shoff + 2 * 64 + 32]
                .try_into()
                .expect("8"),
        ))
        .expect("fits");
        bad_name[verneed_off + 4..verneed_off + 8].copy_from_slice(&0xffff_u32.to_le_bytes());
        assert!(matches!(
            version_needs(&bad_name),
            Err(ElfVersionError::Malformed(_))
        ));
        // Bit flips anywhere never panic.
        for at in 0..elf.len() {
            let mut flipped = elf.clone();
            flipped[at] ^= 0xff;
            let _ = version_needs(&flipped);
        }
    }

    /// The reader agrees with binutils on a real binary: this test executable, when `objdump` is
    /// installed (the farm hosts have it; a machine without it checks the fixture only).
    #[cfg(target_os = "linux")]
    #[test]
    fn max_glibc_needed_matches_objdump_on_this_binary() {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let bytes = std::fs::read(&exe).expect("read the test binary");
        let native = max_glibc_needed(&bytes).expect("the test binary parses");
        if cfg!(target_env = "gnu") {
            assert!(native.is_some(), "a glibc test binary needs GLIBC_ versions");
        }
        let Some(text) =
            crate::process::try_output(std::process::Command::new("objdump").arg("-T").arg(&exe))
        else {
            eprintln!("objdump not installed: compared against the fixture only");
            return;
        };
        let from_objdump = crate::commands::ship_check::max_glibc_version(&text);
        // `objdump -T` also prints versions the binary defines; a Rust executable defines none.
        assert_eq!(
            native,
            from_objdump,
            "native reader vs objdump -T on {}",
            exe.display()
        );
    }
}
