// SPDX-License-Identifier: MPL-2.0
//! Bounds-checked readers for the three object formats a dynamic extension ships in
//! (`specs/57` § 2.8, ADR-2844).
//!
//! The host proves an extension matches its build **before** the dynamic loader maps anything, so
//! it must read the library as bytes. These readers take `&[u8]` and never panic, index out of
//! range or allocate in proportion to a claimed count: every offset, size and count in the file is
//! checked against the slice with checked arithmetic, and every loop is bounded by the bytes that
//! exist. They are small on purpose: they answer two questions only.
//!
//! * [`ObjFile::exported_data`]: the bytes of an exported **data** symbol (ELF `.dynsym`, the PE
//!   export table, the Mach-O `LC_SYMTAB` external symbols), found through the file's own segment
//!   or section mapping rather than by position.
//! * [`ObjFile::needed_libraries`]: the libraries the file asks the loader for (ELF `DT_NEEDED`,
//!   PE import descriptors, Mach-O `LC_LOAD_DYLIB` and its weak, re-export and upward forms).
//!
//! Supported shapes: ELF64 little-endian shared objects and executables, PE32+ images, thin
//! Mach-O 64-bit little-endian images. Universal (fat) Mach-O files, 32-bit images, big-endian
//! ELF and ELF files without section headers are refused with [`ObjError::Unsupported`].
//! Everything follows the public format documents (see `docs/provenance.md`).

use std::fmt;

/// The container format of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjFormat {
    /// ELF64, little-endian.
    Elf64,
    /// PE32+ (a 64-bit Windows image).
    Pe64,
    /// Mach-O, 64-bit, little-endian, thin.
    MachO64,
}

impl ObjFormat {
    /// A name for messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Elf64 => "ELF64",
            Self::Pe64 => "PE32+",
            Self::MachO64 => "Mach-O 64",
        }
    }
}

/// Why a file could not be read as an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjError {
    /// The bytes start with none of the three magics.
    NotObject,
    /// A real object of a shape these readers do not handle.
    Unsupported(&'static str),
    /// A table, offset or count points outside the file, or is inconsistent.
    Malformed(&'static str),
    /// The symbol is not exported (or is only imported).
    SymbolMissing,
    /// The symbol exists but its recorded size is not the size asked for (ELF only).
    SymbolSize {
        /// Size in the file.
        found: u64,
        /// Size asked for.
        expected: usize,
    },
}

impl fmt::Display for ObjError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotObject => write!(f, "not an ELF, PE or Mach-O file"),
            Self::Unsupported(what) => write!(f, "unsupported object file: {what}"),
            Self::Malformed(what) => write!(f, "malformed object file: {what}"),
            Self::SymbolMissing => write!(f, "the symbol is not exported"),
            Self::SymbolSize { found, expected } => {
                write!(f, "the symbol is {found} bytes, {expected} expected")
            }
        }
    }
}

impl std::error::Error for ObjError {}

/// The longest symbol or library name a reader follows.
const MAX_NAME: usize = 4096;
/// Section, segment and load-command counts above this are refused (real files have dozens).
const MAX_TABLE: usize = 1 << 16;
/// Entries walked in one symbol, export or import table.
const MAX_ENTRIES: usize = 1 << 22;

fn le<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..at.checked_add(N)?)?.try_into().ok()
}
fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    le::<2>(b, at).map(u16::from_le_bytes)
}
fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    le::<4>(b, at).map(u32::from_le_bytes)
}
fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    le::<8>(b, at).map(u64::from_le_bytes)
}
fn to_usize(value: u64) -> Option<usize> {
    usize::try_from(value).ok()
}
/// `len` bytes at 64-bit offset `at`, inside `b`.
fn span(b: &[u8], at: u64, len: u64) -> Option<&[u8]> {
    let start = to_usize(at)?;
    let end = to_usize(at.checked_add(len)?)?;
    b.get(start..end)
}
/// The NUL-terminated name starting at `at`, bounded by `MAX_NAME` and the slice.
fn name_at(b: &[u8], at: usize) -> Option<&[u8]> {
    let rest = b.get(at..)?;
    let window = &rest[..rest.len().min(MAX_NAME)];
    let end = window.iter().position(|c| *c == 0)?;
    Some(&window[..end])
}
fn lossy(name: &[u8]) -> String {
    String::from_utf8_lossy(name).into_owned()
}

/// A parsed file: the format and the bytes.
#[derive(Debug, Clone, Copy)]
pub struct ObjFile<'a> {
    format: ObjFormat,
    bytes: &'a [u8],
}

impl<'a> ObjFile<'a> {
    /// Recognizes the container and checks the header fields the readers rely on.
    ///
    /// # Errors
    ///
    /// [`ObjError::NotObject`] for unknown magic; [`ObjError::Unsupported`] or
    /// [`ObjError::Malformed`] for a recognized container the readers cannot follow.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, ObjError> {
        if bytes.starts_with(&[0x7f, b'E', b'L', b'F']) {
            elf::check_header(bytes)?;
            Ok(Self {
                format: ObjFormat::Elf64,
                bytes,
            })
        } else if bytes.starts_with(b"MZ") {
            pe::Pe::parse(bytes)?;
            Ok(Self {
                format: ObjFormat::Pe64,
                bytes,
            })
        } else if bytes.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]) || bytes.starts_with(&[0xBE, 0xBA, 0xFE, 0xCA])
        {
            Err(ObjError::Unsupported(
                "universal Mach-O file; build and ship a thin library per architecture",
            ))
        } else if u32_at(bytes, 0) == Some(macho::MH_MAGIC_64) {
            macho::check_header(bytes)?;
            Ok(Self {
                format: ObjFormat::MachO64,
                bytes,
            })
        } else {
            Err(ObjError::NotObject)
        }
    }

    /// The container format.
    #[must_use]
    pub fn format(&self) -> ObjFormat {
        self.format
    }

    /// The `len` bytes of the exported data symbol `name`, read through the file's own address
    /// mapping. On Mach-O the symbol may carry the C leading underscore.
    ///
    /// # Errors
    ///
    /// [`ObjError::SymbolMissing`] when no defined, exported symbol has the name;
    /// [`ObjError::SymbolSize`] for an ELF symbol of another size; [`ObjError::Malformed`] when
    /// the symbol's address is not backed by file bytes.
    pub fn exported_data(&self, name: &str, len: usize) -> Result<&'a [u8], ObjError> {
        match self.format {
            ObjFormat::Elf64 => elf::exported_data(self.bytes, name.as_bytes(), len),
            ObjFormat::Pe64 => pe::Pe::parse(self.bytes)?.exported_data(name.as_bytes(), len),
            ObjFormat::MachO64 => macho::exported_data(self.bytes, name.as_bytes(), len),
        }
    }

    /// The libraries the file asks the dynamic loader for, in file order (paths as written).
    ///
    /// # Errors
    ///
    /// [`ObjError::Malformed`] when a table points outside the file.
    pub fn needed_libraries(&self) -> Result<Vec<String>, ObjError> {
        match self.format {
            ObjFormat::Elf64 => elf::needed(self.bytes),
            ObjFormat::Pe64 => pe::Pe::parse(self.bytes)?.needed(),
            ObjFormat::MachO64 => macho::needed(self.bytes),
        }
    }
}

/// Whether a library name is Rust's dynamic standard library: `libstd-<hash>.so` (ELF),
/// `std-<hash>.dll` (PE), `libstd-<hash>.dylib` (Mach-O). Directory parts are ignored and the
/// comparison is case-insensitive for PE names.
#[must_use]
pub fn is_dynamic_std_name(library: &str) -> bool {
    let file = library
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(library)
        .to_ascii_lowercase();
    let hashed = |rest: &str| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit());
    if let Some(rest) = file.strip_prefix("libstd-") {
        return rest
            .strip_suffix(".so")
            .or_else(|| rest.strip_suffix(".dylib"))
            .is_some_and(hashed);
    }
    file.strip_prefix("std-")
        .and_then(|rest| rest.strip_suffix(".dll"))
        .is_some_and(hashed)
}

/// Whether an executable's file bytes ask the loader for the dynamic standard library. This is
/// the check that works on every platform; `/proc/self/maps` answers it on Linux only.
///
/// # Errors
///
/// The reader's [`ObjError`] when the bytes are not a supported object.
pub fn links_dynamic_std(executable: &[u8]) -> Result<bool, ObjError> {
    let file = ObjFile::parse(executable)?;
    Ok(file
        .needed_libraries()?
        .iter()
        .any(|name| is_dynamic_std_name(name)))
}

mod elf {
    use super::{MAX_ENTRIES, MAX_TABLE, ObjError, lossy, name_at, span, to_usize, u16_at, u32_at, u64_at};

    const SHT_DYNSYM: u32 = 11;
    const SHT_DYNAMIC: u32 = 6;
    const PT_LOAD: u32 = 1;
    const DT_NEEDED: u64 = 1;
    const SYM_SIZE: u64 = 24;

    struct Section {
        kind: u32,
        offset: u64,
        size: u64,
        link: u32,
        entsize: u64,
    }

    pub(super) fn check_header(b: &[u8]) -> Result<(), ObjError> {
        if b.get(4) != Some(&2) {
            return Err(ObjError::Unsupported("32-bit ELF"));
        }
        if b.get(5) != Some(&1) {
            return Err(ObjError::Unsupported("big-endian ELF"));
        }
        if b.len() < 64 {
            return Err(ObjError::Malformed("ELF header is cut short"));
        }
        match u16_at(b, 16) {
            Some(2 | 3) => Ok(()),
            _ => Err(ObjError::Unsupported(
                "ELF file is neither a shared object nor an executable",
            )),
        }
    }

    fn section(b: &[u8], index: u64) -> Option<Section> {
        let shoff = u64_at(b, 40)?;
        let entsize = u64::from(u16_at(b, 58)?);
        let count = u64::from(u16_at(b, 60)?);
        if entsize < 64 || index >= count {
            return None;
        }
        let at = shoff.checked_add(index.checked_mul(entsize)?)?;
        let h = span(b, at, 64)?;
        Some(Section {
            kind: u32_at(h, 4)?,
            offset: u64_at(h, 24)?,
            size: u64_at(h, 32)?,
            link: u32_at(h, 40)?,
            entsize: u64_at(h, 56)?,
        })
    }

    fn section_count(b: &[u8]) -> Result<u64, ObjError> {
        let count = u64::from(u16_at(b, 60).ok_or(ObjError::Malformed("ELF header is cut short"))?);
        if count == 0 {
            return Err(ObjError::Unsupported(
                "ELF file without section headers (stripped with sstrip?)",
            ));
        }
        if usize::try_from(count).map_or(true, |c| c > MAX_TABLE) {
            return Err(ObjError::Malformed("too many ELF sections"));
        }
        Ok(count)
    }

    /// File offset of `len` bytes at virtual address `vaddr`, through the `PT_LOAD` segments.
    fn file_offset(b: &[u8], vaddr: u64, len: u64) -> Option<u64> {
        let phoff = u64_at(b, 32)?;
        let entsize = u64::from(u16_at(b, 54)?);
        let count = u64::from(u16_at(b, 56)?);
        if entsize < 56 {
            return None;
        }
        for i in 0..count {
            let h = span(b, phoff.checked_add(i.checked_mul(entsize)?)?, 56)?;
            if u32_at(h, 0)? != PT_LOAD {
                continue;
            }
            let (p_offset, p_vaddr, p_filesz) = (u64_at(h, 8)?, u64_at(h, 16)?, u64_at(h, 32)?);
            let delta = vaddr.checked_sub(p_vaddr)?;
            if delta.checked_add(len)? <= p_filesz {
                return p_offset.checked_add(delta);
            }
        }
        None
    }

    fn table<'a>(b: &'a [u8], s: &Section) -> Result<&'a [u8], ObjError> {
        span(b, s.offset, s.size).ok_or(ObjError::Malformed("an ELF table lies outside the file"))
    }

    pub(super) fn exported_data<'a>(b: &'a [u8], name: &[u8], len: usize) -> Result<&'a [u8], ObjError> {
        let count = section_count(b)?;
        for index in 0..count {
            let sh =
                section(b, index).ok_or(ObjError::Malformed("a section header lies outside the file"))?;
            if sh.kind != SHT_DYNSYM {
                continue;
            }
            if sh.entsize != SYM_SIZE {
                return Err(ObjError::Malformed("dynamic symbol entries are not 24 bytes"));
            }
            let strings =
                section(b, u64::from(sh.link)).ok_or(ObjError::Malformed("no string table for .dynsym"))?;
            let strings = table(b, &strings)?;
            let symbols = table(b, &sh)?;
            let n = to_usize(sh.size / SYM_SIZE).unwrap_or(0).min(MAX_ENTRIES);
            for i in 0..n {
                let s = &symbols[i * 24..i * 24 + 24];
                let (Some(name_idx), Some(info), Some(shndx), Some(value), Some(size)) = (
                    u32_at(s, 0),
                    s.get(4).copied(),
                    u16_at(s, 6),
                    u64_at(s, 8),
                    u64_at(s, 16),
                ) else {
                    continue;
                };
                // Defined, global or weak, a data object.
                if shndx == 0 || !matches!(info >> 4, 1 | 2) || info & 0xf != 1 {
                    continue;
                }
                let Some(candidate) = to_usize(u64::from(name_idx)).and_then(|at| name_at(strings, at))
                else {
                    continue;
                };
                if candidate != name {
                    continue;
                }
                if size != len as u64 {
                    return Err(ObjError::SymbolSize {
                        found: size,
                        expected: len,
                    });
                }
                let at = file_offset(b, value, size).ok_or(ObjError::Malformed(
                    "the symbol's address is not in a loadable segment of the file",
                ))?;
                return span(b, at, size)
                    .ok_or(ObjError::Malformed("the symbol's bytes lie outside the file"));
            }
        }
        Err(ObjError::SymbolMissing)
    }

    pub(super) fn needed(b: &[u8]) -> Result<Vec<String>, ObjError> {
        let count = section_count(b)?;
        let mut out = Vec::new();
        for index in 0..count {
            let sh =
                section(b, index).ok_or(ObjError::Malformed("a section header lies outside the file"))?;
            if sh.kind != SHT_DYNAMIC {
                continue;
            }
            let strings =
                section(b, u64::from(sh.link)).ok_or(ObjError::Malformed("no string table for .dynamic"))?;
            let strings = table(b, &strings)?;
            let entries = table(b, &sh)?;
            for entry in entries.as_chunks::<16>().0.iter().take(MAX_ENTRIES) {
                let (Some(tag), Some(value)) = (u64_at(entry, 0), u64_at(entry, 8)) else {
                    break;
                };
                if tag == 0 {
                    break;
                }
                if tag == DT_NEEDED {
                    let name =
                        to_usize(value)
                            .and_then(|at| name_at(strings, at))
                            .ok_or(ObjError::Malformed(
                                "a DT_NEEDED name lies outside the string table",
                            ))?;
                    out.push(lossy(name));
                }
            }
        }
        Ok(out)
    }
}

mod pe {
    use super::{MAX_ENTRIES, ObjError, lossy, name_at, span, to_usize, u16_at, u32_at};

    struct Sect {
        virtual_size: u32,
        virtual_address: u32,
        raw_size: u32,
        raw_ptr: u32,
    }

    pub(super) struct Pe<'a> {
        b: &'a [u8],
        sections: Vec<Sect>,
        export: (u32, u32),
        import: (u32, u32),
    }

    impl<'a> Pe<'a> {
        pub(super) fn parse(b: &'a [u8]) -> Result<Self, ObjError> {
            const CUT: ObjError = ObjError::Malformed("PE header is cut short");
            let pe_at = to_usize(u64::from(u32_at(b, 0x3c).ok_or(CUT)?)).ok_or(CUT)?;
            if b.get(pe_at..pe_at.saturating_add(4)) != Some(b"PE\0\0") {
                return Err(ObjError::Malformed("no PE signature at e_lfanew"));
            }
            let coff = pe_at + 4;
            let section_count = usize::from(u16_at(b, coff + 2).ok_or(CUT)?);
            let optional_size = usize::from(u16_at(b, coff + 16).ok_or(CUT)?);
            let opt = coff + 20;
            match u16_at(b, opt) {
                Some(0x20b) => {}
                Some(0x10b) => return Err(ObjError::Unsupported("32-bit PE image")),
                _ => return Err(ObjError::Malformed("unknown PE optional header")),
            }
            if section_count > 96 {
                return Err(ObjError::Malformed("too many PE sections"));
            }
            let dir_count = u32_at(b, opt + 108).ok_or(CUT)?;
            let dir = |index: usize| -> (u32, u32) {
                if index >= dir_count as usize || 112 + index * 8 + 8 > optional_size {
                    return (0, 0);
                }
                let at = opt + 112 + index * 8;
                (u32_at(b, at).unwrap_or(0), u32_at(b, at + 4).unwrap_or(0))
            };
            let first = opt.checked_add(optional_size).ok_or(CUT)?;
            let mut sections = Vec::with_capacity(section_count);
            for i in 0..section_count {
                let h = span(b, (first + i * 40) as u64, 40)
                    .ok_or(ObjError::Malformed("a PE section header lies outside the file"))?;
                sections.push(Sect {
                    virtual_size: u32_at(h, 8).ok_or(CUT)?,
                    virtual_address: u32_at(h, 12).ok_or(CUT)?,
                    raw_size: u32_at(h, 16).ok_or(CUT)?,
                    raw_ptr: u32_at(h, 20).ok_or(CUT)?,
                });
            }
            Ok(Self {
                b,
                sections,
                export: dir(0),
                import: dir(1),
            })
        }

        /// File offset of `len` bytes at `rva`, inside one section's raw data.
        fn offset(&self, rva: u64, len: u64) -> Option<usize> {
            for s in &self.sections {
                let start = u64::from(s.virtual_address);
                let extent = u64::from(s.virtual_size.max(s.raw_size));
                if rva < start || rva >= start + extent {
                    continue;
                }
                let delta = rva - start;
                if delta.checked_add(len)? > u64::from(s.raw_size) {
                    return None;
                }
                let at = u64::from(s.raw_ptr).checked_add(delta)?;
                span(self.b, at, len)?;
                return to_usize(at);
            }
            None
        }

        fn u32_rva(&self, rva: u64) -> Option<u32> {
            u32_at(self.b, self.offset(rva, 4)?)
        }

        fn name_rva(&self, rva: u64) -> Option<&'a [u8]> {
            name_at(self.b, self.offset(rva, 1)?)
        }

        pub(super) fn exported_data(&self, name: &[u8], len: usize) -> Result<&'a [u8], ObjError> {
            let (dir_rva, dir_size) = self.export;
            if dir_rva == 0 {
                return Err(ObjError::SymbolMissing);
            }
            const BAD: ObjError = ObjError::Malformed("the PE export table lies outside the file");
            let at = self.offset(u64::from(dir_rva), 40).ok_or(BAD)?;
            let field = |off: usize| u32_at(self.b, at + off).ok_or(BAD);
            let functions = u64::from(field(20)?);
            let names = u64::from(field(24)?).min(MAX_ENTRIES as u64);
            let function_table = u64::from(field(28)?);
            let name_table = u64::from(field(32)?);
            let ordinal_table = u64::from(field(36)?);
            for i in 0..names {
                let Some(name_rva) = self.u32_rva(name_table + 4 * i) else {
                    return Err(BAD);
                };
                let Some(candidate) = self.name_rva(u64::from(name_rva)) else {
                    return Err(BAD);
                };
                if candidate != name {
                    continue;
                }
                let ordinal = self
                    .offset(ordinal_table + 2 * i, 2)
                    .and_then(|o| u16_at(self.b, o))
                    .ok_or(BAD)?;
                if u64::from(ordinal) >= functions {
                    return Err(ObjError::Malformed("export ordinal is out of range"));
                }
                let rva = u64::from(self.u32_rva(function_table + 4 * u64::from(ordinal)).ok_or(BAD)?);
                if rva >= u64::from(dir_rva) && rva < u64::from(dir_rva) + u64::from(dir_size) {
                    return Err(ObjError::Malformed("the export is a forwarder, not data"));
                }
                let from = self.offset(rva, len as u64).ok_or(ObjError::Malformed(
                    "the symbol's bytes are not backed by the file",
                ))?;
                return self
                    .b
                    .get(from..from + len)
                    .ok_or(ObjError::Malformed("the symbol's bytes lie outside the file"));
            }
            Err(ObjError::SymbolMissing)
        }

        pub(super) fn needed(&self) -> Result<Vec<String>, ObjError> {
            let (rva, _) = self.import;
            let mut out = Vec::new();
            if rva == 0 {
                return Ok(out);
            }
            const BAD: ObjError = ObjError::Malformed("the PE import table lies outside the file");
            for i in 0..4096u64 {
                let at = self.offset(u64::from(rva) + i * 20, 20).ok_or(BAD)?;
                let name_rva = u32_at(self.b, at + 12).ok_or(BAD)?;
                let all_zero = self.b[at..at + 20].iter().all(|c| *c == 0);
                if all_zero || name_rva == 0 {
                    break;
                }
                let name = self.name_rva(u64::from(name_rva)).ok_or(BAD)?;
                out.push(lossy(name));
            }
            Ok(out)
        }
    }
}

mod macho {
    use super::{MAX_ENTRIES, MAX_TABLE, ObjError, lossy, name_at, span, to_usize, u32_at, u64_at};

    pub(super) const MH_MAGIC_64: u32 = 0xFEED_FACF;
    const LC_SEGMENT_64: u32 = 0x19;
    const LC_SYMTAB: u32 = 0x2;
    const LC_LOAD_DYLIB: u32 = 0xC;
    const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
    const LC_REEXPORT_DYLIB: u32 = 0x8000_001F;
    const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;

    /// Calls `visit(cmd, bytes)` for each load command; `bytes` is the whole command.
    fn commands<'a>(
        b: &'a [u8],
        mut visit: impl FnMut(u32, &'a [u8]) -> Result<(), ObjError>,
    ) -> Result<(), ObjError> {
        const CUT: ObjError = ObjError::Malformed("Mach-O header is cut short");
        let ncmds = u32_at(b, 16).ok_or(CUT)? as usize;
        let total = u32_at(b, 20).ok_or(CUT)? as usize;
        if ncmds > MAX_TABLE {
            return Err(ObjError::Malformed("too many Mach-O load commands"));
        }
        let end = 32usize
            .checked_add(total)
            .filter(|end| *end <= b.len())
            .ok_or(ObjError::Malformed("Mach-O load commands lie outside the file"))?;
        let mut at = 32usize;
        for _ in 0..ncmds {
            const BAD: ObjError = ObjError::Malformed("a Mach-O load command is cut short");
            let cmd = u32_at(b, at).ok_or(BAD)?;
            let size = u32_at(b, at + 4).ok_or(BAD)? as usize;
            let next = at
                .checked_add(size)
                .filter(|n| size >= 8 && *n <= end)
                .ok_or(BAD)?;
            visit(cmd, &b[at..next])?;
            at = next;
        }
        Ok(())
    }

    pub(super) fn check_header(b: &[u8]) -> Result<(), ObjError> {
        if b.len() < 32 {
            return Err(ObjError::Malformed("Mach-O header is cut short"));
        }
        Ok(())
    }

    struct Segment {
        vmaddr: u64,
        fileoff: u64,
        filesize: u64,
    }

    fn file_offset(segments: &[Segment], vmaddr: u64, len: u64) -> Option<u64> {
        segments.iter().find_map(|s| {
            let delta = vmaddr.checked_sub(s.vmaddr)?;
            (delta.checked_add(len)? <= s.filesize).then(|| s.fileoff.checked_add(delta))?
        })
    }

    pub(super) fn exported_data<'a>(b: &'a [u8], name: &[u8], len: usize) -> Result<&'a [u8], ObjError> {
        let mut segments = Vec::new();
        let mut symtab = None;
        commands(b, |cmd, c| {
            match cmd {
                LC_SEGMENT_64 => segments.push(Segment {
                    vmaddr: u64_at(c, 24).ok_or(ObjError::Malformed("short segment command"))?,
                    fileoff: u64_at(c, 40).ok_or(ObjError::Malformed("short segment command"))?,
                    filesize: u64_at(c, 48).ok_or(ObjError::Malformed("short segment command"))?,
                }),
                LC_SYMTAB => {
                    const BAD: ObjError = ObjError::Malformed("short symtab command");
                    symtab = Some((
                        u64::from(u32_at(c, 8).ok_or(BAD)?),
                        u32_at(c, 12).ok_or(BAD)? as usize,
                        u64::from(u32_at(c, 16).ok_or(BAD)?),
                        u64::from(u32_at(c, 20).ok_or(BAD)?),
                    ));
                }
                _ => {}
            }
            Ok(())
        })?;
        let (symoff, nsyms, stroff, strsize) = symtab.ok_or(ObjError::SymbolMissing)?;
        let strings = span(b, stroff, strsize).ok_or(ObjError::Malformed(
            "the Mach-O string table lies outside the file",
        ))?;
        let mut underscored = Vec::with_capacity(name.len() + 1);
        underscored.push(b'_');
        underscored.extend_from_slice(name);
        for i in 0..nsyms.min(MAX_ENTRIES) {
            let Some(entry) = span(b, symoff + (i as u64) * 16, 16) else {
                return Err(ObjError::Malformed(
                    "the Mach-O symbol table lies outside the file",
                ));
            };
            let (Some(strx), Some(n_type), Some(n_sect), Some(value)) = (
                u32_at(entry, 0),
                entry.get(4).copied(),
                entry.get(5).copied(),
                u64_at(entry, 8),
            ) else {
                continue;
            };
            // Defined in a section (N_SECT), external (N_EXT), not a debugging entry.
            if n_type & 0xE0 != 0 || n_type & 0x0E != 0x0E || n_type & 1 == 0 || n_sect == 0 {
                continue;
            }
            let Some(candidate) = to_usize(u64::from(strx)).and_then(|at| name_at(strings, at)) else {
                continue;
            };
            if candidate != name && candidate != underscored.as_slice() {
                continue;
            }
            let at = file_offset(&segments, value, len as u64).ok_or(ObjError::Malformed(
                "the symbol's address is not in a segment of the file",
            ))?;
            return span(b, at, len as u64)
                .ok_or(ObjError::Malformed("the symbol's bytes lie outside the file"));
        }
        Err(ObjError::SymbolMissing)
    }

    pub(super) fn needed(b: &[u8]) -> Result<Vec<String>, ObjError> {
        let mut out = Vec::new();
        commands(b, |cmd, c| {
            if matches!(
                cmd,
                LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB | LC_LOAD_UPWARD_DYLIB
            ) {
                const BAD: ObjError = ObjError::Malformed("a dylib name lies outside its command");
                let at = u32_at(c, 8).ok_or(BAD)? as usize;
                out.push(lossy(name_at(c, at).ok_or(BAD)?));
            }
            Ok(())
        })?;
        Ok(out)
    }
}

/// Byte builders for synthetic, minimal ELF64, PE32+ and Mach-O 64 files. They write just the
/// tables the readers follow, so tests (and the fuzz seeds) need no compiler or linker. Not a
/// writer for real libraries.
pub mod synth {
    fn pad(out: &mut Vec<u8>, align: usize) {
        while !out.len().is_multiple_of(align) {
            out.push(0);
        }
    }
    fn put_u16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn put_u64(out: &mut Vec<u8>, v: u64) {
        out.extend_from_slice(&v.to_le_bytes());
    }
    fn set_u32(out: &mut [u8], at: usize, v: u32) {
        out[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// A shared object exporting each `(name, data)` as an `STT_OBJECT` of `data.len()` bytes and
    /// needing each of `needed`. One `PT_LOAD` maps the file at virtual address `0x10000`.
    #[must_use]
    pub fn elf64(symbols: &[(&str, &[u8])], needed: &[&str]) -> Vec<u8> {
        const BASE: u64 = 0x10000;
        let mut out = vec![0u8; 64 + 56];
        // Data objects.
        let mut placed = Vec::new();
        for (_, data) in symbols {
            pad(&mut out, 16);
            placed.push(out.len() as u64);
            out.extend_from_slice(data);
        }
        // .dynstr
        pad(&mut out, 8);
        let dynstr_off = out.len();
        out.push(0);
        let mut needed_idx = Vec::new();
        for lib in needed {
            needed_idx.push((out.len() - dynstr_off) as u64);
            out.extend_from_slice(lib.as_bytes());
            out.push(0);
        }
        let mut name_idx = Vec::new();
        for (name, _) in symbols {
            name_idx.push((out.len() - dynstr_off) as u32);
            out.extend_from_slice(name.as_bytes());
            out.push(0);
        }
        let dynstr_size = out.len() - dynstr_off;
        // .dynsym: the null symbol, then one per export.
        pad(&mut out, 8);
        let dynsym_off = out.len();
        out.extend_from_slice(&[0; 24]);
        for (i, (_, data)) in symbols.iter().enumerate() {
            put_u32(&mut out, name_idx[i]);
            out.push(0x11); // GLOBAL, OBJECT
            out.push(0);
            put_u16(&mut out, 1); // defined in section 1
            put_u64(&mut out, BASE + placed[i]);
            put_u64(&mut out, data.len() as u64);
        }
        let dynsym_size = out.len() - dynsym_off;
        // .dynamic
        pad(&mut out, 8);
        let dynamic_off = out.len();
        for idx in &needed_idx {
            put_u64(&mut out, 1);
            put_u64(&mut out, *idx);
        }
        put_u64(&mut out, 0);
        put_u64(&mut out, 0);
        let dynamic_size = out.len() - dynamic_off;
        // Section headers: null, .dynsym, .dynstr, .dynamic.
        pad(&mut out, 8);
        let shoff = out.len();
        let header = |kind: u32, offset: usize, size: usize, link: u32, entsize: u64| {
            let mut h = Vec::new();
            put_u32(&mut h, 0);
            put_u32(&mut h, kind);
            put_u64(&mut h, 0);
            put_u64(&mut h, BASE + offset as u64);
            put_u64(&mut h, offset as u64);
            put_u64(&mut h, size as u64);
            put_u32(&mut h, link);
            put_u32(&mut h, 0);
            put_u64(&mut h, 8);
            put_u64(&mut h, entsize);
            h
        };
        let sections = [
            vec![0u8; 64],
            header(11, dynsym_off, dynsym_size, 2, 24),
            header(3, dynstr_off, dynstr_size, 0, 0),
            header(6, dynamic_off, dynamic_size, 2, 16),
        ];
        for s in &sections {
            out.extend_from_slice(s);
        }
        let total = out.len() as u64;
        out[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        out[4] = 2;
        out[5] = 1;
        out[6] = 1;
        out[16..18].copy_from_slice(&3u16.to_le_bytes());
        out[18..20].copy_from_slice(&0x3eu16.to_le_bytes());
        out[20..24].copy_from_slice(&1u32.to_le_bytes());
        out[32..40].copy_from_slice(&64u64.to_le_bytes());
        out[40..48].copy_from_slice(&(shoff as u64).to_le_bytes());
        out[52..54].copy_from_slice(&64u16.to_le_bytes());
        out[54..56].copy_from_slice(&56u16.to_le_bytes());
        out[56..58].copy_from_slice(&1u16.to_le_bytes());
        out[58..60].copy_from_slice(&64u16.to_le_bytes());
        out[60..62].copy_from_slice(&4u16.to_le_bytes());
        // The one PT_LOAD: the whole file at BASE.
        let mut ph = Vec::new();
        put_u32(&mut ph, 1);
        put_u32(&mut ph, 5);
        put_u64(&mut ph, 0);
        put_u64(&mut ph, BASE);
        put_u64(&mut ph, BASE);
        put_u64(&mut ph, total);
        put_u64(&mut ph, total);
        put_u64(&mut ph, 0x1000);
        out[64..120].copy_from_slice(&ph);
        out
    }

    /// A PE32+ DLL with one `.rdata` section exporting each `(name, data)` as data and importing
    /// each of `imports` (one dummy function each).
    #[must_use]
    pub fn pe64(exports: &[(&str, &[u8])], imports: &[&str]) -> Vec<u8> {
        const RAW: usize = 0x200;
        const VA: u32 = 0x1000;
        let rva = |rel: usize| VA + rel as u32;
        let mut s: Vec<u8> = Vec::new();
        // Data first.
        let mut data_rva = Vec::new();
        for (_, data) in exports {
            pad(&mut s, 16);
            data_rva.push(rva(s.len()));
            s.extend_from_slice(data);
        }
        // Export directory (40 bytes), function, name and ordinal tables, strings.
        pad(&mut s, 8);
        let dir_rel = s.len();
        s.extend_from_slice(&[0; 40]);
        let functions_rel = s.len();
        for r in &data_rva {
            put_u32(&mut s, *r);
        }
        let names_rel = s.len();
        s.resize(s.len() + 4 * exports.len(), 0);
        let ordinals_rel = s.len();
        for i in 0..exports.len() {
            put_u16(&mut s, i as u16);
        }
        pad(&mut s, 4);
        let dll_rel = s.len();
        s.extend_from_slice(b"synth.dll\0");
        for (i, (name, _)) in exports.iter().enumerate() {
            let at = s.len();
            s.extend_from_slice(name.as_bytes());
            s.push(0);
            set_u32(&mut s, names_rel + 4 * i, rva(at));
        }
        let export_size = s.len() - dir_rel;
        set_u32(&mut s, dir_rel + 12, rva(dll_rel));
        set_u32(&mut s, dir_rel + 16, 1);
        set_u32(&mut s, dir_rel + 20, exports.len() as u32);
        set_u32(&mut s, dir_rel + 24, exports.len() as u32);
        set_u32(&mut s, dir_rel + 28, rva(functions_rel));
        set_u32(&mut s, dir_rel + 32, rva(names_rel));
        set_u32(&mut s, dir_rel + 36, rva(ordinals_rel));
        // Import descriptors.
        pad(&mut s, 8);
        let import_rel = s.len();
        s.resize(s.len() + 20 * (imports.len() + 1), 0);
        for (i, lib) in imports.iter().enumerate() {
            let at = s.len();
            s.extend_from_slice(lib.as_bytes());
            s.push(0);
            set_u32(&mut s, import_rel + 20 * i + 12, rva(at));
        }
        let import_size = 20 * (imports.len() + 1);
        // Headers.
        let mut out = vec![0u8; RAW];
        out[0..2].copy_from_slice(b"MZ");
        out[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        out[0x40..0x44].copy_from_slice(b"PE\0\0");
        let coff = 0x44;
        out[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes());
        out[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        out[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes());
        out[coff + 18..coff + 20].copy_from_slice(&0x2022u16.to_le_bytes());
        let opt = coff + 20;
        out[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes());
        out[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
        out[opt + 112..opt + 116].copy_from_slice(&rva(dir_rel).to_le_bytes());
        out[opt + 116..opt + 120].copy_from_slice(&(export_size as u32).to_le_bytes());
        out[opt + 120..opt + 124].copy_from_slice(&rva(import_rel).to_le_bytes());
        out[opt + 124..opt + 128].copy_from_slice(&(import_size as u32).to_le_bytes());
        let sec = opt + 240;
        out[sec..sec + 6].copy_from_slice(b".rdata");
        out[sec + 8..sec + 12].copy_from_slice(&(s.len() as u32).to_le_bytes());
        out[sec + 12..sec + 16].copy_from_slice(&VA.to_le_bytes());
        out[sec + 16..sec + 20].copy_from_slice(&(s.len() as u32).to_le_bytes());
        out[sec + 20..sec + 24].copy_from_slice(&(RAW as u32).to_le_bytes());
        out.extend_from_slice(&s);
        out
    }

    /// A thin Mach-O 64 dylib exporting each `(name, data)` (stored with the C underscore, as
    /// `ld` writes it) and loading each of `dylibs`.
    #[must_use]
    pub fn macho64(symbols: &[(&str, &[u8])], dylibs: &[&str]) -> Vec<u8> {
        const BASE: u64 = 0x1_0000_0000;
        let dylib_cmd_size = |lib: &str| (24 + lib.len() + 1).next_multiple_of(8);
        let cmds_size = 72 + 24 + dylibs.iter().map(|l| dylib_cmd_size(l)).sum::<usize>();
        let mut body = Vec::new();
        let start = 32 + cmds_size;
        let mut placed = Vec::new();
        for (_, data) in symbols {
            pad(&mut body, 16);
            placed.push((start + body.len()) as u64);
            body.extend_from_slice(data);
        }
        pad(&mut body, 8);
        let symoff = start + body.len();
        let mut strings = vec![0u8];
        let mut nlists = Vec::new();
        for (i, (name, _)) in symbols.iter().enumerate() {
            put_u32(&mut nlists, strings.len() as u32);
            nlists.push(0x0f); // N_SECT | N_EXT
            nlists.push(1);
            put_u16(&mut nlists, 0);
            put_u64(&mut nlists, BASE + placed[i]);
            strings.push(b'_');
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        body.extend_from_slice(&nlists);
        let stroff = start + body.len();
        body.extend_from_slice(&strings);
        let total = start + body.len();

        let mut out = Vec::new();
        put_u32(&mut out, 0xFEED_FACF);
        put_u32(&mut out, 0x0100_000C); // arm64
        put_u32(&mut out, 0);
        put_u32(&mut out, 6); // MH_DYLIB
        put_u32(&mut out, 2 + dylibs.len() as u32);
        put_u32(&mut out, cmds_size as u32);
        put_u32(&mut out, 0);
        put_u32(&mut out, 0);
        // LC_SEGMENT_64, no sections: the whole file.
        put_u32(&mut out, 0x19);
        put_u32(&mut out, 72);
        out.extend_from_slice(b"__TEXT\0\0\0\0\0\0\0\0\0\0");
        put_u64(&mut out, BASE);
        put_u64(&mut out, total as u64);
        put_u64(&mut out, 0);
        put_u64(&mut out, total as u64);
        put_u32(&mut out, 7);
        put_u32(&mut out, 5);
        put_u32(&mut out, 0);
        put_u32(&mut out, 0);
        // LC_SYMTAB
        put_u32(&mut out, 2);
        put_u32(&mut out, 24);
        put_u32(&mut out, symoff as u32);
        put_u32(&mut out, symbols.len() as u32);
        put_u32(&mut out, stroff as u32);
        put_u32(&mut out, strings.len() as u32);
        for lib in dylibs {
            let size = dylib_cmd_size(lib);
            put_u32(&mut out, 0xC);
            put_u32(&mut out, size as u32);
            put_u32(&mut out, 24);
            put_u32(&mut out, 2);
            put_u32(&mut out, 0x0001_0000);
            put_u32(&mut out, 0x0001_0000);
            out.extend_from_slice(lib.as_bytes());
            out.push(0);
            out.resize(out.len() + (size - 24 - lib.len() - 1), 0);
        }
        debug_assert_eq!(out.len(), start);
        out.extend_from_slice(&body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::synth::{elf64, macho64, pe64};
    use super::*;

    const NAME: &str = "REGIA_EXTENSION_STAMP";
    const DATA: [u8; 24] = *b"0123456789abcdefghijklmn";

    fn samples() -> [(ObjFormat, Vec<u8>); 3] {
        [
            (
                ObjFormat::Elf64,
                elf64(
                    &[("other", b"zz"), (NAME, &DATA)],
                    &["libc.so.6", "libstd-0123abcd.so"],
                ),
            ),
            (
                ObjFormat::Pe64,
                pe64(
                    &[("other", b"zz"), (NAME, &DATA)],
                    &["KERNEL32.dll", "std-0123abcd.dll"],
                ),
            ),
            (
                ObjFormat::MachO64,
                macho64(
                    &[("other", b"zz"), (NAME, &DATA)],
                    &["/usr/lib/libSystem.B.dylib", "@rpath/libstd-0123abcd.dylib"],
                ),
            ),
        ]
    }

    #[test]
    fn each_format_yields_the_exported_bytes_and_its_libraries() {
        for (format, bytes) in samples() {
            let file = ObjFile::parse(&bytes).expect("parses");
            assert_eq!(file.format(), format);
            assert_eq!(
                file.exported_data(NAME, DATA.len()).expect("symbol"),
                &DATA,
                "{format:?}"
            );
            assert_eq!(
                file.exported_data("other", 2).expect("symbol"),
                b"zz",
                "{format:?}"
            );
            assert_eq!(
                file.exported_data("missing", 2),
                Err(ObjError::SymbolMissing),
                "{format:?}"
            );
            let libs = file.needed_libraries().expect("libraries");
            assert_eq!(libs.len(), 2, "{format:?}: {libs:?}");
            assert!(
                libs.iter().any(|l| is_dynamic_std_name(l)),
                "{format:?}: {libs:?}"
            );
            assert_eq!(links_dynamic_std(&bytes), Ok(true), "{format:?}");
        }
    }

    #[test]
    fn a_file_without_the_std_library_is_not_dynamic_std() {
        for bytes in [
            elf64(&[(NAME, &DATA)], &["libc.so.6"]),
            pe64(&[(NAME, &DATA)], &["KERNEL32.dll"]),
            macho64(&[(NAME, &DATA)], &["/usr/lib/libSystem.B.dylib"]),
        ] {
            assert_eq!(links_dynamic_std(&bytes), Ok(false));
        }
        assert_eq!(links_dynamic_std(b"just text"), Err(ObjError::NotObject));
    }

    #[test]
    fn std_names_are_matched_by_shape() {
        for yes in [
            "libstd-0123abcd.so",
            "/lib/libstd-ab12.so",
            "std-ab12.dll",
            "C:\\x\\STD-AB12.DLL",
            "@rpath/libstd-ab12.dylib",
        ] {
            assert!(is_dynamic_std_name(yes), "{yes}");
        }
        for no in [
            "libstd.so",
            "libstdc++.so.6",
            "libstd-.so",
            "libstd-xyz.so",
            "std-ab12.so",
            "libstd_detect-ab.so",
            "mystd-ab12.dll",
            "libstd-ab12.dll",
        ] {
            assert!(!is_dynamic_std_name(no), "{no}");
        }
    }

    #[test]
    fn a_symbol_of_the_wrong_size_is_refused_on_elf() {
        let bytes = elf64(&[(NAME, &DATA)], &[]);
        let file = ObjFile::parse(&bytes).expect("parses");
        assert_eq!(
            file.exported_data(NAME, 32),
            Err(ObjError::SymbolSize {
                found: 24,
                expected: 32
            })
        );
    }

    #[test]
    fn macho_accepts_the_plain_or_underscored_name() {
        let bytes = macho64(&[(NAME, &DATA)], &[]);
        let file = ObjFile::parse(&bytes).expect("parses");
        assert!(file.exported_data(NAME, 24).is_ok());
        assert!(
            file.exported_data("_REGIA_EXTENSION_STAMP", 24).is_ok(),
            "the stored name matches as written"
        );
        assert!(
            file.exported_data("__REGIA_EXTENSION_STAMP", 24).is_err(),
            "no other underscore count"
        );
    }

    #[test]
    fn a_string_that_merely_contains_the_bytes_is_not_the_symbol() {
        // H3: the bytes sit in the file (as a decoy string), but the exported symbol names other
        // bytes. The reader answers with the symbol's bytes.
        let real = *b"REAL_BYTES_REAL_BYTES___";
        let decoy = *b"DECOY_BYTES_DECOY_BYTES_";
        for (format, mut bytes) in [
            (ObjFormat::Elf64, elf64(&[(NAME, &real)], &[])),
            (ObjFormat::Pe64, pe64(&[(NAME, &real)], &[])),
            (ObjFormat::MachO64, macho64(&[(NAME, &real)], &[])),
        ] {
            // The decoy follows every table, so a byte scan for it finds it too.
            bytes.extend_from_slice(&decoy);
            let file = ObjFile::parse(&bytes).expect("parses");
            assert_eq!(file.exported_data(NAME, 24).expect("symbol"), &real, "{format:?}");
            assert_ne!(file.exported_data(NAME, 24).expect("symbol"), &decoy);
        }
    }

    #[test]
    fn unsupported_shapes_are_named() {
        let mut thirty_two = elf64(&[], &[]);
        thirty_two[4] = 1;
        assert!(matches!(
            ObjFile::parse(&thirty_two),
            Err(ObjError::Unsupported(_))
        ));
        let mut big = elf64(&[], &[]);
        big[5] = 2;
        assert!(matches!(ObjFile::parse(&big), Err(ObjError::Unsupported(_))));
        let mut pe32 = pe64(&[], &[]);
        pe32[0x44 + 20..0x44 + 22].copy_from_slice(&0x10bu16.to_le_bytes());
        assert!(matches!(ObjFile::parse(&pe32), Err(ObjError::Unsupported(_))));
        assert!(matches!(
            ObjFile::parse(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 2]),
            Err(ObjError::Unsupported(_))
        ));
        let mut no_sections = elf64(&[(NAME, &DATA)], &[]);
        no_sections[60..62].copy_from_slice(&0u16.to_le_bytes());
        let file = ObjFile::parse(&no_sections).expect("header ok");
        assert!(matches!(
            file.exported_data(NAME, 24),
            Err(ObjError::Unsupported(_))
        ));
    }

    /// Every prefix of every sample, and the samples with each byte flipped to a few values, are
    /// read without a panic; the answer is either a value or an error.
    #[test]
    fn truncated_and_flipped_files_never_panic() {
        for (_, bytes) in samples() {
            for len in 0..bytes.len() {
                let cut = &bytes[..len];
                if let Ok(file) = ObjFile::parse(cut) {
                    let _ = file.exported_data(NAME, 24);
                    let _ = file.needed_libraries();
                }
            }
            for at in 0..bytes.len() {
                for value in [0x00, 0xff, 0x7f, 0x80] {
                    let mut copy = bytes.clone();
                    copy[at] = value;
                    if let Ok(file) = ObjFile::parse(&copy) {
                        let _ = file.exported_data(NAME, 24);
                        let _ = file.needed_libraries();
                    }
                }
            }
        }
    }

    /// Deterministic pseudo-random garbage behind a valid magic.
    #[test]
    fn hostile_tables_are_refused_not_followed() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for magic in [
            vec![0x7f, b'E', b'L', b'F', 2, 1, 1, 0],
            b"MZ".to_vec(),
            vec![0xCF, 0xFA, 0xED, 0xFE],
        ] {
            for _ in 0..200 {
                let mut bytes = magic.clone();
                let n = (next() % 600) as usize;
                bytes.extend((0..n).map(|_| next().to_le_bytes()[0]));
                if let Ok(file) = ObjFile::parse(&bytes) {
                    let _ = file.exported_data(NAME, 24);
                    let _ = file.needed_libraries();
                }
            }
        }
    }

    #[test]
    fn huge_claimed_counts_are_bounded() {
        // ELF: a .dynsym claiming u64::MAX / 24 entries at a valid offset.
        let mut elf = elf64(&[(NAME, &DATA)], &[]);
        let shoff = u64::from_le_bytes(elf[40..48].try_into().expect("8 bytes")) as usize;
        elf[shoff + 64 + 32..shoff + 64 + 40].copy_from_slice(&u64::MAX.to_le_bytes());
        let file = ObjFile::parse(&elf).expect("header ok");
        assert!(matches!(
            file.exported_data(NAME, 24),
            Err(ObjError::Malformed(_))
        ));
        // Mach-O: ncmds past the cap.
        let mut macho = macho64(&[], &[]);
        macho[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let file = ObjFile::parse(&macho).expect("header ok");
        assert!(matches!(file.needed_libraries(), Err(ObjError::Malformed(_))));
        // PE: a name table claiming 2^32 names.
        let mut pe = pe64(&[(NAME, &DATA)], &[]);
        let dir = 0x200;
        let export_rva =
            u32::from_le_bytes(pe[0x44 + 20 + 112..0x44 + 20 + 116].try_into().expect("4 bytes")) as usize;
        let at = dir + (export_rva - 0x1000);
        pe[at + 24..at + 28].copy_from_slice(&u32::MAX.to_le_bytes());
        let file = ObjFile::parse(&pe).expect("header ok");
        assert!(file.exported_data("nothing_like_it", 24).is_err());
    }

    #[test]
    fn a_pe_forwarder_is_not_data() {
        let mut pe = pe64(&[(NAME, &DATA)], &[]);
        // Point the function entry into the export directory itself.
        let export_rva =
            u32::from_le_bytes(pe[0x44 + 20 + 112..0x44 + 20 + 116].try_into().expect("4 bytes"));
        let dir_at = 0x200 + (export_rva as usize - 0x1000);
        let functions_rva = u32::from_le_bytes(pe[dir_at + 28..dir_at + 32].try_into().expect("4 bytes"));
        let at = 0x200 + (functions_rva as usize - 0x1000);
        pe[at..at + 4].copy_from_slice(&export_rva.to_le_bytes());
        let file = ObjFile::parse(&pe).expect("parses");
        assert!(matches!(
            file.exported_data(NAME, 24),
            Err(ObjError::Malformed(_))
        ));
    }

    #[test]
    fn a_symbol_address_outside_the_file_is_malformed() {
        let mut elf = elf64(&[(NAME, &DATA)], &[]);
        // Move the PT_LOAD so the symbol's address is not inside it.
        elf[64 + 16..64 + 24].copy_from_slice(&0x90_0000u64.to_le_bytes());
        let file = ObjFile::parse(&elf).expect("parses");
        assert!(matches!(
            file.exported_data(NAME, 24),
            Err(ObjError::Malformed(_))
        ));
    }
}
