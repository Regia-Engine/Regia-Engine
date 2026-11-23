// SPDX-License-Identifier: MPL-2.0
//! `dist_binary`: binary checks for `cargo xtask dist`, read in-process so they run on any host
//! (a Windows exe is checked on the Linux machine that cross-built it):
//!
//! - Windows: the DLLs a PE32+ exe imports (import and delay-load directories) and its subsystem,
//!   from Microsoft's "PE Format" specification. Each DLL is classed as a Windows system DLL, a
//!   Visual C++ runtime DLL, or unknown; an unknown DLL that is not in the bundle fails the bundle.
//! - Steam Deck: the `DT_NEEDED` libraries and the `GLIBC_x.y` versions an x86-64 ELF requires,
//!   from the System V ABI (section headers, `.dynamic`) and the Linux Standard Base
//!   (`SHT_GNU_verneed`). The newest glibc must not exceed the Steam Linux Runtime 3.0 floor, 2.31.
//!
//! Provenance: `docs/provenance.md` (original code from the public specifications).

use anyhow::{Context, Result, bail};

/// glibc of Steam Linux Runtime 3.0 "sniper", the runtime Steam and the Steam Deck run native Linux
/// games in. Kept equal to `regia_cli`'s `STEAM_RUNTIME_GLIBC` (a test reads that file).
pub const STEAM_RUNTIME_GLIBC: [u32; 2] = [2, 31];

/// Shared libraries Steam Linux Runtime 3.0 provides (sonames). Kept equal to `regia_cli`'s
/// `STEAM_RUNTIME_LIBS` (a test reads that file).
pub const STEAM_RUNTIME_LIBS: &[&str] = &[
    "ld-linux-x86-64.so.2",
    "libc.so.6",
    "libm.so.6",
    "libdl.so.2",
    "libpthread.so.0",
    "librt.so.1",
    "libgcc_s.so.1",
    "libstdc++.so.6",
    "libz.so.1",
    "libasound.so.2",
    "libpulse.so.0",
    "libudev.so.1",
    "libdbus-1.so.3",
    "libX11.so.6",
    "libX11-xcb.so.1",
    "libxcb.so.1",
    "libXcursor.so.1",
    "libXrandr.so.2",
    "libXi.so.6",
    "libxkbcommon.so.0",
    "libxkbcommon-x11.so.0",
    "libwayland-client.so.0",
    "libwayland-cursor.so.0",
    "libwayland-egl.so.1",
    "libvulkan.so.1",
    "libsteam_api.so",
];

/// DLLs that ship with every supported Windows (10 and 11, x64), lower case. Anything named
/// `api-ms-win-*` or `ext-ms-win-*` (API sets, which include the Universal CRT) is a system DLL too.
pub const WINDOWS_SYSTEM_DLLS: &[&str] = &[
    "advapi32.dll",
    "avrt.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "cfgmgr32.dll",
    "combase.dll",
    "comctl32.dll",
    "comdlg32.dll",
    "crypt32.dll",
    "d2d1.dll",
    "d3d11.dll",
    "d3d12.dll",
    "d3dcompiler_47.dll",
    "dbghelp.dll",
    "dinput8.dll",
    "dsound.dll",
    "dwmapi.dll",
    "dwrite.dll",
    "dxgi.dll",
    "gdi32.dll",
    "hid.dll",
    "imm32.dll",
    "iphlpapi.dll",
    "kernel32.dll",
    "kernelbase.dll",
    "mfplat.dll",
    "mmdevapi.dll",
    "msimg32.dll",
    "msvcrt.dll",
    "mswsock.dll",
    "ncrypt.dll",
    "netapi32.dll",
    "normaliz.dll",
    "ntdll.dll",
    "ole32.dll",
    "oleacc.dll",
    "oleaut32.dll",
    "opengl32.dll",
    "powrprof.dll",
    "propsys.dll",
    "psapi.dll",
    "rpcrt4.dll",
    "secur32.dll",
    "sechost.dll",
    "setupapi.dll",
    "shcore.dll",
    "shell32.dll",
    "shlwapi.dll",
    "synchronization.dll",
    "ucrtbase.dll",
    "urlmon.dll",
    "user32.dll",
    "userenv.dll",
    "uxtheme.dll",
    "version.dll",
    "windowscodecs.dll",
    "winhttp.dll",
    "wininet.dll",
    "winmm.dll",
    "wintrust.dll",
    "ws2_32.dll",
    "wtsapi32.dll",
    "xinput1_4.dll",
];

/// The Visual C++ runtime DLLs a dynamically linked MSVC exe needs (the "Microsoft Visual C++
/// Redistributable", present on most gaming PCs, not on a fresh Windows install).
pub const MSVC_RUNTIME_DLLS: &[&str] = &[
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "msvcp140.dll",
    "msvcp140_1.dll",
    "msvcp140_2.dll",
    "concrt140.dll",
];

/// What a DLL import is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DllClass {
    /// Part of Windows (including API sets and the Universal CRT).
    System,
    /// The Visual C++ runtime (needs the redistributable unless linked with `+crt-static`).
    MsvcRuntime,
    /// The GPU driver installs it (`vulkan-1.dll`).
    GpuDriver,
    /// Shipped in the bundle next to the exe.
    Bundled,
    /// None of the above: the exe does not start on a machine without it.
    Unknown,
}

impl DllClass {
    /// The lower-case name used in `dist-manifest.json`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::MsvcRuntime => "msvc_runtime",
            Self::GpuDriver => "gpu_driver",
            Self::Bundled => "bundled",
            Self::Unknown => "unknown",
        }
    }
}

/// Classes `dll` (any case); `bundled` holds the lower-case file names shipped next to the exe.
#[must_use]
pub fn classify_dll(dll: &str, bundled: &[String]) -> DllClass {
    let lower = dll.to_ascii_lowercase();
    if bundled.contains(&lower) {
        DllClass::Bundled
    } else if lower.starts_with("api-ms-win-")
        || lower.starts_with("ext-ms-win-")
        || WINDOWS_SYSTEM_DLLS.contains(&lower.as_str())
    {
        DllClass::System
    } else if MSVC_RUNTIME_DLLS.contains(&lower.as_str()) {
        DllClass::MsvcRuntime
    } else if lower == "vulkan-1.dll" {
        DllClass::GpuDriver
    } else {
        DllClass::Unknown
    }
}

/// What [`read_pe`] found in a Windows executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeReport {
    /// COFF machine (`0x8664` for x64).
    pub machine: u16,
    /// Optional-header subsystem: 2 = Windows GUI, 3 = console.
    pub subsystem: u16,
    /// DLLs of the import directory, in file order.
    pub imports: Vec<String>,
    /// DLLs of the delay-load import directory, in file order.
    pub delay_imports: Vec<String>,
}

impl PeReport {
    /// `"windows"` (GUI), `"console"` or `"other"`.
    #[must_use]
    pub fn subsystem_label(&self) -> &'static str {
        match self.subsystem {
            2 => "windows",
            3 => "console",
            _ => "other",
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    b.get(at..at + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .with_context(|| format!("truncated at byte {at}"))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .with_context(|| format!("truncated at byte {at}"))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64> {
    b.get(at..at + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .with_context(|| format!("truncated at byte {at}"))
}

/// A NUL-terminated string at `at` (at most 4096 bytes).
fn c_str(b: &[u8], at: usize) -> Result<String> {
    let tail = b
        .get(at..)
        .with_context(|| format!("string offset {at} out of range"))?;
    let len = tail
        .iter()
        .take(4096)
        .position(|&c| c == 0)
        .with_context(|| format!("unterminated string at {at}"))?;
    String::from_utf8(tail[..len].to_vec()).with_context(|| format!("string at {at} is not UTF-8"))
}

/// One PE section: virtual address, virtual size, raw size, raw offset.
struct Section {
    va: u32,
    vsize: u32,
    raw_size: u32,
    raw_ptr: u32,
}

/// File offset of `rva`, through the section that maps it.
fn rva_to_offset(sections: &[Section], rva: u32) -> Result<usize> {
    for s in sections {
        let span = s.vsize.max(s.raw_size);
        if rva >= s.va && rva - s.va < span {
            return Ok((u64::from(rva - s.va) + u64::from(s.raw_ptr)) as usize);
        }
    }
    bail!("RVA {rva:#x} is in no section")
}

/// Reads the machine, subsystem and imported DLLs of a PE32 or PE32+ image.
///
/// # Errors
/// Not a PE image, or a truncated or inconsistent header or import table.
pub fn read_pe(bytes: &[u8]) -> Result<PeReport> {
    if bytes.get(..2) != Some(b"MZ".as_slice()) {
        bail!("not a Windows executable (no MZ header)");
    }
    let pe = u32_at(bytes, 0x3C)? as usize;
    if bytes.get(pe..pe + 4) != Some(b"PE\0\0".as_slice()) {
        bail!("not a PE image (no PE signature at {pe:#x})");
    }
    let coff = pe + 4;
    let machine = u16_at(bytes, coff)?;
    let sections_n = usize::from(u16_at(bytes, coff + 2)?);
    let optional_size = usize::from(u16_at(bytes, coff + 16)?);
    let opt = coff + 20;
    let magic = u16_at(bytes, opt)?;
    let (count_at, dirs_at) = match magic {
        0x20b => (opt + 108, opt + 112), // PE32+
        0x10b => (opt + 92, opt + 96),   // PE32
        other => bail!("unknown optional header magic {other:#x}"),
    };
    let subsystem = u16_at(bytes, opt + 68)?;
    let dir_count = u32_at(bytes, count_at)? as usize;
    let dir = |index: usize| -> Result<(u32, u32)> {
        if index >= dir_count {
            return Ok((0, 0));
        }
        Ok((
            u32_at(bytes, dirs_at + index * 8)?,
            u32_at(bytes, dirs_at + index * 8 + 4)?,
        ))
    };
    let section_table = opt + optional_size;
    let mut sections = Vec::with_capacity(sections_n);
    for i in 0..sections_n {
        let s = section_table + i * 40;
        sections.push(Section {
            vsize: u32_at(bytes, s + 8)?,
            va: u32_at(bytes, s + 12)?,
            raw_size: u32_at(bytes, s + 16)?,
            raw_ptr: u32_at(bytes, s + 20)?,
        });
    }

    // Import directory (index 1): 20-byte descriptors, the DLL name RVA at +12, ended by zeros.
    let mut imports = Vec::new();
    let (import_rva, _) = dir(1)?;
    if import_rva != 0 {
        let mut at = rva_to_offset(&sections, import_rva)?;
        loop {
            let descriptor = bytes.get(at..at + 20).context("truncated import directory")?;
            if descriptor.iter().all(|&b| b == 0) {
                break;
            }
            let name_rva = u32_at(bytes, at + 12)?;
            imports.push(c_str(bytes, rva_to_offset(&sections, name_rva)?)?);
            at += 20;
            if imports.len() > 4096 {
                bail!("import directory has no terminator");
            }
        }
    }
    // Delay-load directory (index 13): 32-byte descriptors, the DLL name RVA at +4.
    let mut delay_imports = Vec::new();
    let (delay_rva, _) = dir(13)?;
    if delay_rva != 0 {
        let mut at = rva_to_offset(&sections, delay_rva)?;
        loop {
            let descriptor = bytes.get(at..at + 32).context("truncated delay-load directory")?;
            if descriptor.iter().all(|&b| b == 0) {
                break;
            }
            let name_rva = u32_at(bytes, at + 4)?;
            delay_imports.push(c_str(bytes, rva_to_offset(&sections, name_rva)?)?);
            at += 32;
            if delay_imports.len() > 4096 {
                bail!("delay-load directory has no terminator");
            }
        }
    }
    Ok(PeReport {
        machine,
        subsystem,
        imports,
        delay_imports,
    })
}

/// What [`read_elf`] found in a Linux executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfReport {
    /// ELF machine (`62` for x86-64).
    pub machine: u16,
    /// `DT_NEEDED` sonames, in file order.
    pub needed: Vec<String>,
    /// Every `GLIBC_*` version the binary requires (from `SHT_GNU_verneed`), sorted, unique.
    pub glibc_versions: Vec<Vec<u32>>,
}

impl ElfReport {
    /// The newest glibc version required, if any.
    #[must_use]
    pub fn max_glibc(&self) -> Option<&[u32]> {
        self.glibc_versions.last().map(Vec::as_slice)
    }

    /// `NEEDED` libraries the Steam Linux Runtime does not provide.
    #[must_use]
    pub fn outside_runtime(&self) -> Vec<String> {
        self.needed
            .iter()
            .filter(|lib| !STEAM_RUNTIME_LIBS.contains(&lib.as_str()))
            .cloned()
            .collect()
    }
}

/// `GLIBC_2.31` → `[2, 31]`, `GLIBC_2.2.5` → `[2, 2, 5]`; `GLIBC_PRIVATE` and other names → `None`.
#[must_use]
pub fn parse_glibc_version(name: &str) -> Option<Vec<u32>> {
    let version = name.strip_prefix("GLIBC_")?;
    version.split('.').map(|part| part.parse().ok()).collect()
}

/// `[2, 31]` → `"2.31"`.
#[must_use]
pub fn version_text(version: &[u32]) -> String {
    version.iter().map(u32::to_string).collect::<Vec<_>>().join(".")
}

/// Reads the `DT_NEEDED` libraries and required glibc versions of a 64-bit little-endian ELF
/// through its section headers (`strip --strip-all` keeps them, and `.dynamic`, `.dynstr` and
/// `.gnu.version_r`).
///
/// # Errors
/// Not a 64-bit little-endian ELF, or a truncated or inconsistent section table.
pub fn read_elf(bytes: &[u8]) -> Result<ElfReport> {
    if bytes.get(..4) != Some(b"\x7fELF".as_slice()) {
        bail!("not an ELF file");
    }
    if bytes.get(4) != Some(&2) || bytes.get(5) != Some(&1) {
        bail!("not a 64-bit little-endian ELF");
    }
    let machine = u16_at(bytes, 0x12)?;
    let shoff = u64_at(bytes, 0x28)? as usize;
    let shentsize = usize::from(u16_at(bytes, 0x3A)?);
    let shnum = usize::from(u16_at(bytes, 0x3C)?);
    if shoff == 0 || shnum == 0 || shoff > bytes.len() {
        bail!("ELF has no section headers");
    }
    if shentsize < 64 {
        bail!("ELF section header size {shentsize} < 64");
    }
    // (type, offset, size, link, info) of each section.
    let mut sections = Vec::with_capacity(shnum);
    for i in 0..shnum {
        let s = shoff + i * shentsize;
        sections.push((
            u32_at(bytes, s + 4)?,
            u64_at(bytes, s + 24)? as usize,
            u64_at(bytes, s + 32)? as usize,
            u32_at(bytes, s + 40)? as usize,
            u32_at(bytes, s + 44)? as usize,
        ));
    }
    let string_table = |link: usize| -> Result<usize> {
        sections
            .get(link)
            .map(|s| s.1)
            .with_context(|| format!("section link {link} out of range"))
    };
    const SHT_DYNAMIC: u32 = 6;
    const SHT_GNU_VERNEED: u32 = 0x6fff_fffe;
    const DT_NULL: u64 = 0;
    const DT_NEEDED: u64 = 1;

    let mut needed = Vec::new();
    let mut glibc_versions = Vec::new();
    for &(kind, offset, size, link, info) in &sections {
        match kind {
            SHT_DYNAMIC => {
                let strtab = string_table(link)?;
                for at in (offset..offset.saturating_add(size).min(bytes.len())).step_by(16) {
                    let tag = u64_at(bytes, at)?;
                    if tag == DT_NULL {
                        break;
                    }
                    if tag == DT_NEEDED {
                        let name = u64_at(bytes, at + 8)? as usize;
                        needed.push(c_str(bytes, strtab.saturating_add(name))?);
                    }
                }
            }
            SHT_GNU_VERNEED => {
                let strtab = string_table(link)?;
                // `info` Verneed entries: vn_cnt @2, vn_aux @8, vn_next @12. Each Vernaux: vna_name
                // @8, vna_next @12.
                let mut entry = offset;
                for _ in 0..info {
                    if entry > bytes.len() {
                        bail!("verneed entry offset {entry} out of range");
                    }
                    let count = u16_at(bytes, entry + 2)?;
                    let mut aux = entry + u32_at(bytes, entry + 8)? as usize;
                    for _ in 0..count {
                        if aux > bytes.len() {
                            bail!("vernaux offset {aux} out of range");
                        }
                        let name = c_str(bytes, strtab.saturating_add(u32_at(bytes, aux + 8)? as usize))?;
                        if let Some(version) = parse_glibc_version(&name) {
                            glibc_versions.push(version);
                        }
                        let next = u32_at(bytes, aux + 12)? as usize;
                        if next == 0 {
                            break;
                        }
                        aux += next;
                    }
                    let next = u32_at(bytes, entry + 12)? as usize;
                    if next == 0 {
                        break;
                    }
                    entry += next;
                }
            }
            _ => {}
        }
    }
    glibc_versions.sort();
    glibc_versions.dedup();
    Ok(ElfReport {
        machine,
        needed,
        glibc_versions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glibc_versions_parse_and_order() {
        assert_eq!(parse_glibc_version("GLIBC_2.31"), Some(vec![2, 31]));
        assert_eq!(parse_glibc_version("GLIBC_2.2.5"), Some(vec![2, 2, 5]));
        assert_eq!(parse_glibc_version("GLIBC_PRIVATE"), None);
        assert_eq!(parse_glibc_version("GCC_3.0"), None);
        // Element-wise order: 2.2.5 < 2.17 < 2.31 < 2.34.
        let v = |name: &str| parse_glibc_version(name).unwrap_or_default();
        assert!(v("GLIBC_2.2.5") < v("GLIBC_2.17"));
        assert!(v("GLIBC_2.17") < v("GLIBC_2.31"));
        assert!(v("GLIBC_2.31") < v("GLIBC_2.34"));
        assert!(v("GLIBC_2.34").as_slice() > STEAM_RUNTIME_GLIBC.as_slice());
        assert!(v("GLIBC_2.2.5").as_slice() <= STEAM_RUNTIME_GLIBC.as_slice());
        assert!(v("GLIBC_2.31").as_slice() <= STEAM_RUNTIME_GLIBC.as_slice());
        assert_eq!(version_text(&[2, 2, 5]), "2.2.5");
    }

    #[test]
    fn dlls_are_classed() {
        let bundled = vec!["steam_api64.dll".to_string()];
        assert_eq!(classify_dll("KERNEL32.dll", &bundled), DllClass::System);
        assert_eq!(
            classify_dll("api-ms-win-crt-runtime-l1-1-0.dll", &bundled),
            DllClass::System
        );
        assert_eq!(classify_dll("VCRUNTIME140.dll", &bundled), DllClass::MsvcRuntime);
        assert_eq!(classify_dll("vulkan-1.dll", &bundled), DllClass::GpuDriver);
        assert_eq!(classify_dll("steam_api64.dll", &bundled), DllClass::Bundled);
        assert_eq!(classify_dll("libfoo.dll", &bundled), DllClass::Unknown);
        assert_eq!(DllClass::MsvcRuntime.label(), "msvc_runtime");
    }

    #[test]
    fn windows_audio_device_api_is_a_system_dependency() {
        // The real shipping showcase imports ActivateAudioInterfaceAsync from this Windows DLL.
        assert_eq!(classify_dll("mmdevapi.dll", &[]), DllClass::System);
        assert_eq!(classify_dll("MMDEVAPI.DLL", &[]), DllClass::System);
        assert_eq!(classify_dll("mmdevapi_hook.dll", &[]), DllClass::Unknown);
    }

    #[test]
    fn non_pe_and_non_elf_bytes_are_refused() {
        assert!(read_pe(b"\x7fELF....").is_err());
        assert!(read_pe(b"MZ").is_err());
        assert!(read_elf(b"MZ\0\0").is_err());
        let mut elf32 = b"\x7fELF".to_vec();
        elf32.extend_from_slice(&[1, 1, 1, 0]);
        assert!(read_elf(&elf32).is_err());
    }

    #[test]
    fn steam_runtime_lists_match_regia_cli() -> Result<()> {
        // `regia package` (ship_check.rs) and `xtask dist` must agree on the Steam runtime.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/regia_cli/src/commands/ship_check.rs");
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let start = text
            .find("pub const STEAM_RUNTIME_LIBS")
            .context("STEAM_RUNTIME_LIBS not found in ship_check.rs")?;
        let block = &text[start..];
        let block = &block[..block.find("];").context("unterminated STEAM_RUNTIME_LIBS")?];
        let theirs: Vec<&str> = block.split('"').skip(1).step_by(2).collect();
        assert_eq!(
            theirs, STEAM_RUNTIME_LIBS,
            "keep the two Steam runtime lists equal"
        );
        assert!(
            text.contains("GlibcVersion { major: 2, minor: 31 }"),
            "ship_check's STEAM_RUNTIME_GLIBC moved off 2.31; update STEAM_RUNTIME_GLIBC here"
        );
        Ok(())
    }
}
