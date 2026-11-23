// SPDX-License-Identifier: MPL-2.0
//! Android symbolication artifact (`docs/specs/21-tooling-and-packaging.md` § 4 and § 5, ADR-2631).
//!
//! AGP strips the native libraries while it packages the APK, so a shipping `regia package` copies the
//! unstripped `lib<name>.so` of each ABI out of the cargo-ndk output (`jniLibs/<abi>/`) before Gradle
//! runs:
//!
//! ```text
//! dist/android/symbols/
//!   symbols.json                 # format_version 1: version, build number, one entry per ABI
//!   arm64-v8a/libmain.so         # unstripped, the build id of the one in the APK
//!   x86_64/libmain.so
//! ```
//!
//! `symbols.json` records the GNU build id of each library; a crash report's build id picks the
//! matching `symbols/` directory of that release. `ndk-stack -sym symbols/<abi>` and
//! `llvm-addr2line -e symbols/<abi>/lib<name>.so` read the files directly.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// `format_version` of `symbols.json`.
pub const SYMBOLS_FORMAT_VERSION: u32 = 1;

/// One library of `symbols.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolLibrary {
    /// Android ABI directory name (`arm64-v8a`).
    pub abi: String,
    /// Path relative to `symbols/` (`arm64-v8a/libmain.so`).
    pub file: String,
    /// Lowercase hex GNU build id, `None` when the library carries none.
    pub build_id: Option<String>,
    /// Size in bytes of the unstripped library.
    pub size: u64,
}

/// `dist/android/symbols/symbols.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolsManifest {
    /// Always [`SYMBOLS_FORMAT_VERSION`].
    pub format_version: u32,
    /// `[project] version`.
    pub version: String,
    /// `[project] build_number` (Android `versionCode`).
    pub build_number: u32,
    /// The libraries, in manifest ABI order.
    pub libraries: Vec<SymbolLibrary>,
}

/// What [`inspect_elf`] reads from a library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfInfo {
    /// GNU build id as lowercase hex.
    pub build_id: Option<String>,
    /// The file still has a symbol table or DWARF sections (nothing useful is left after a strip).
    pub has_symbols: bool,
}

fn read_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn read_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn read_u64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Reads the build id and the symbol state of a little-endian ELF32/ELF64 shared object (every
/// Android ABI). Returns `None` for anything that is not one.
#[must_use]
pub fn inspect_elf(bytes: &[u8]) -> Option<ElfInfo> {
    if bytes.get(0..4)? != b"\x7fELF" || *bytes.get(5)? != 1 {
        return None; // not ELF, or big endian
    }
    let is64 = match *bytes.get(4)? {
        1 => false,
        2 => true,
        _ => return None,
    };
    let word = |at: usize| -> Option<usize> {
        if is64 {
            usize::try_from(read_u64(bytes, at)?).ok()
        } else {
            Some(read_u32(bytes, at)? as usize)
        }
    };
    let (shoff, shentsize, shnum, shstrndx) = if is64 {
        (
            word(0x28)?,
            read_u16(bytes, 0x3A)? as usize,
            read_u16(bytes, 0x3C)? as usize,
            read_u16(bytes, 0x3E)? as usize,
        )
    } else {
        (
            word(0x20)?,
            read_u16(bytes, 0x2E)? as usize,
            read_u16(bytes, 0x30)? as usize,
            read_u16(bytes, 0x32)? as usize,
        )
    };
    if shoff == 0 || shnum == 0 || shstrndx >= shnum {
        return None;
    }
    // Section header fields: name (u32 at 0), offset, size.
    let (off_at, size_at) = if is64 { (0x18, 0x20) } else { (0x10, 0x14) };
    let section = |index: usize| -> Option<(u32, usize, usize)> {
        let base = shoff.checked_add(index.checked_mul(shentsize)?)?;
        Some((
            read_u32(bytes, base)?,
            word(base + off_at)?,
            word(base + size_at)?,
        ))
    };
    let (_, str_off, str_size) = section(shstrndx)?;
    let names = bytes.get(str_off..str_off.checked_add(str_size)?)?;
    let name_of = |offset: u32| -> &[u8] {
        let tail = names.get(offset as usize..).unwrap_or(&[]);
        tail.split(|b| *b == 0).next().unwrap_or(&[])
    };

    let mut build_id = None;
    let mut has_symbols = false;
    for index in 0..shnum {
        let (name, off, size) = section(index)?;
        let name = name_of(name);
        if name == b".symtab" || name.starts_with(b".debug_") {
            has_symbols = true;
        } else if name == b".note.gnu.build-id" {
            let note = bytes.get(off..off.checked_add(size)?)?;
            let namesz = read_u32(note, 0)? as usize;
            let descsz = read_u32(note, 4)? as usize;
            let desc_at = 12usize.checked_add(namesz.next_multiple_of(4))?;
            if read_u32(note, 8)? == 3 && note.get(12..12 + namesz)? == b"GNU\0" {
                let desc = note.get(desc_at..desc_at.checked_add(descsz)?)?;
                build_id = Some(desc.iter().map(|b| format!("{b:02x}")).collect());
            }
        }
    }
    Some(ElfInfo {
        build_id,
        has_symbols,
    })
}

/// Copies the unstripped `lib<lib_name>.so` of every ABI from `jni_libs_dir/<abi>/` into
/// `out_dir/symbols/<abi>/` and writes `out_dir/symbols/symbols.json`. Returns the `symbols`
/// directory.
///
/// Errors (first word is the stable code): `android_symbols_missing` (an ABI has no library),
/// `android_symbols_not_elf` (not a shared object), `android_symbols_stripped` (no symbol table and
/// no DWARF: the shipping profile must keep `strip = "none"`).
pub fn collect_symbols(
    jni_libs_dir: &Path,
    out_dir: &Path,
    abis: &[String],
    lib_name: &str,
    version: &str,
    build_number: u32,
) -> Result<PathBuf> {
    let symbols_dir = out_dir.join("symbols");
    if symbols_dir.exists() {
        std::fs::remove_dir_all(&symbols_dir)
            .with_context(|| format!("removing stale {}", symbols_dir.display()))?;
    }
    let file_name = format!("lib{lib_name}.so");
    let mut libraries = Vec::new();
    for abi in abis {
        let source = jni_libs_dir.join(abi).join(&file_name);
        if !source.is_file() {
            bail!("android_symbols_missing: {} does not exist", source.display());
        }
        let bytes = std::fs::read(&source).with_context(|| format!("reading {}", source.display()))?;
        let Some(info) = inspect_elf(&bytes) else {
            bail!(
                "android_symbols_not_elf: {} is not a little-endian ELF shared object",
                source.display()
            );
        };
        if !info.has_symbols {
            bail!(
                "android_symbols_stripped: {} has no symbol table or debug info; build the shipping \
                 profile with `strip = \"none\"` so the symbols can be kept",
                source.display()
            );
        }
        if info.build_id.is_none() {
            tracing::warn!(abi = %abi, "{} has no GNU build id; symbols.json records null", source.display());
        }
        let dest_dir = symbols_dir.join(abi);
        std::fs::create_dir_all(&dest_dir).with_context(|| format!("creating {}", dest_dir.display()))?;
        std::fs::write(dest_dir.join(&file_name), &bytes)
            .with_context(|| format!("writing {}", dest_dir.join(&file_name).display()))?;
        libraries.push(SymbolLibrary {
            abi: abi.clone(),
            file: format!("{abi}/{file_name}"),
            build_id: info.build_id,
            size: bytes.len() as u64,
        });
    }
    let manifest = SymbolsManifest {
        format_version: SYMBOLS_FORMAT_VERSION,
        version: version.to_owned(),
        build_number,
        libraries,
    };
    let json = serde_json::to_string_pretty(&manifest)?;
    std::fs::write(symbols_dir.join("symbols.json"), json).context("writing symbols.json")?;
    Ok(symbols_dir)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal ELF64 little-endian shared object with the named sections (`.shstrtab` is added).
    pub(crate) fn fake_elf64(build_id: Option<&[u8]>, extra_sections: &[&str]) -> Vec<u8> {
        let mut names = vec![0u8];
        let mut section_names: Vec<(String, u32)> = Vec::new();
        let mut all: Vec<String> = Vec::new();
        if build_id.is_some() {
            all.push(".note.gnu.build-id".into());
        }
        all.extend(extra_sections.iter().map(|s| (*s).to_owned()));
        all.push(".shstrtab".into());
        for n in &all {
            section_names.push((n.clone(), names.len() as u32));
            names.extend_from_slice(n.as_bytes());
            names.push(0);
        }
        let mut note = Vec::new();
        if let Some(id) = build_id {
            note.extend_from_slice(&4u32.to_le_bytes());
            note.extend_from_slice(&(id.len() as u32).to_le_bytes());
            note.extend_from_slice(&3u32.to_le_bytes());
            note.extend_from_slice(b"GNU\0");
            note.extend_from_slice(id);
            while !note.len().is_multiple_of(4) {
                note.push(0);
            }
        }
        let mut out = vec![0u8; 64];
        out[0..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        let note_off = out.len();
        out.extend_from_slice(&note);
        let str_off = out.len();
        out.extend_from_slice(&names);
        while !out.len().is_multiple_of(8) {
            out.push(0);
        }
        let shoff = out.len();
        // Section 0 is the null section.
        out.extend_from_slice(&[0u8; 64]);
        for (name, name_off) in &section_names {
            let (off, size) = if name == ".note.gnu.build-id" {
                (note_off, note.len())
            } else if name == ".shstrtab" {
                (str_off, names.len())
            } else {
                (0, 0)
            };
            let mut header = [0u8; 64];
            header[0..4].copy_from_slice(&name_off.to_le_bytes());
            header[0x18..0x20].copy_from_slice(&(off as u64).to_le_bytes());
            header[0x20..0x28].copy_from_slice(&(size as u64).to_le_bytes());
            out.extend_from_slice(&header);
        }
        let shnum = section_names.len() + 1;
        out[0x28..0x30].copy_from_slice(&(shoff as u64).to_le_bytes());
        out[0x3A..0x3C].copy_from_slice(&64u16.to_le_bytes());
        out[0x3C..0x3E].copy_from_slice(&(shnum as u16).to_le_bytes());
        out[0x3E..0x40].copy_from_slice(&((shnum - 1) as u16).to_le_bytes());
        out
    }

    fn ndk_tree(abis: &[(&str, Vec<u8>)]) -> PathBuf {
        let root = regia_test_utils::unique_temp_dir("android-symbols");
        for (abi, bytes) in abis {
            let dir = root.join("jniLibs").join(abi);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join("libmain.so"), bytes).expect("write lib");
        }
        root
    }

    #[test]
    fn inspect_reads_build_id_and_symbols() {
        let elf = fake_elf64(Some(&[0xde, 0xad, 0xbe, 0xef, 0x01]), &[".symtab", ".debug_line"]);
        let info = inspect_elf(&elf).expect("elf");
        assert_eq!(info.build_id.as_deref(), Some("deadbeef01"));
        assert!(info.has_symbols);
        let stripped = inspect_elf(&fake_elf64(Some(&[1, 2, 3, 4]), &[".text"])).expect("elf");
        assert!(!stripped.has_symbols);
        assert_eq!(
            inspect_elf(&fake_elf64(None, &[".symtab"]))
                .expect("elf")
                .build_id,
            None
        );
    }

    #[test]
    fn inspect_rejects_garbage_without_panicking() {
        assert!(inspect_elf(b"").is_none());
        assert!(inspect_elf(b"not an elf at all, just text").is_none());
        let mut truncated = fake_elf64(Some(&[1, 2, 3, 4]), &[".symtab"]);
        truncated.truncate(100);
        assert!(inspect_elf(&truncated).is_none());
        let mut big_endian = fake_elf64(None, &[]);
        big_endian[5] = 2;
        assert!(inspect_elf(&big_endian).is_none());
    }

    #[test]
    fn collect_copies_each_abi_and_writes_symbols_json() {
        let a = fake_elf64(Some(&[0xaa; 20]), &[".symtab"]);
        let b = fake_elf64(Some(&[0xbb; 20]), &[".debug_info"]);
        let root = ndk_tree(&[("arm64-v8a", a.clone()), ("x86_64", b.clone())]);
        let out = root.join("dist/android");
        let abis = vec!["arm64-v8a".to_owned(), "x86_64".to_owned()];
        let dir = collect_symbols(&root.join("jniLibs"), &out, &abis, "main", "1.2.3", 42).expect("collect");
        assert_eq!(dir, out.join("symbols"));
        assert_eq!(std::fs::read(dir.join("arm64-v8a/libmain.so")).expect("a"), a);
        assert_eq!(std::fs::read(dir.join("x86_64/libmain.so")).expect("b"), b);
        let manifest: SymbolsManifest =
            serde_json::from_slice(&std::fs::read(dir.join("symbols.json")).expect("json")).expect("parse");
        assert_eq!(manifest.format_version, 1);
        assert_eq!((manifest.version.as_str(), manifest.build_number), ("1.2.3", 42));
        assert_eq!(manifest.libraries.len(), 2);
        assert_eq!(manifest.libraries[0].abi, "arm64-v8a");
        assert_eq!(manifest.libraries[0].file, "arm64-v8a/libmain.so");
        assert_eq!(
            manifest.libraries[0].build_id.as_deref(),
            Some("aa".repeat(20).as_str())
        );
        assert_eq!(
            manifest.libraries[1].build_id.as_deref(),
            Some("bb".repeat(20).as_str())
        );
        assert_eq!(manifest.libraries[1].size, b.len() as u64);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_replaces_a_stale_symbols_directory() {
        let root = ndk_tree(&[("arm64-v8a", fake_elf64(Some(&[1; 8]), &[".symtab"]))]);
        let out = root.join("dist/android");
        std::fs::create_dir_all(out.join("symbols/x86_64")).expect("mkdir");
        std::fs::write(out.join("symbols/x86_64/libmain.so"), b"old").expect("old");
        collect_symbols(
            &root.join("jniLibs"),
            &out,
            &["arm64-v8a".to_owned()],
            "main",
            "1",
            1,
        )
        .expect("collect");
        assert!(
            !out.join("symbols/x86_64").exists(),
            "an ABI no longer built is not left behind"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn collect_error_codes() {
        let root = ndk_tree(&[
            ("arm64-v8a", fake_elf64(Some(&[1; 8]), &[".text"])),
            ("x86_64", b"plain text".to_vec()),
        ]);
        let out = root.join("dist/android");
        let jni = root.join("jniLibs");
        let err = |abi: &str| {
            collect_symbols(&jni, &out, &[abi.to_owned()], "main", "1", 1)
                .expect_err("must fail")
                .to_string()
        };
        assert!(err("arm64-v8a").starts_with("android_symbols_stripped:"));
        assert!(err("x86_64").starts_with("android_symbols_not_elf:"));
        assert!(err("armeabi-v7a").starts_with("android_symbols_missing:"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_library_without_build_id_is_recorded_as_null() {
        let root = ndk_tree(&[("arm64-v8a", fake_elf64(None, &[".symtab"]))]);
        let out = root.join("dist/android");
        collect_symbols(
            &root.join("jniLibs"),
            &out,
            &["arm64-v8a".to_owned()],
            "main",
            "1",
            1,
        )
        .expect("collect");
        let text = std::fs::read_to_string(out.join("symbols/symbols.json")).expect("json");
        assert!(text.contains("\"build_id\": null"), "{text}");
        let _ = std::fs::remove_dir_all(root);
    }
}
