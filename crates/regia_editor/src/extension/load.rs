// SPDX-License-Identifier: MPL-2.0
//! Loading a dynamic extension (`specs/57` § 2.8, `specs/20` § 3).
//!
//! Rust has no stable ABI, so a library is accepted only when it proves it matches this engine
//! build. The stamp is a `static` of the library, so the first check reads it from the **file**:
//! a library with no stamp, a corrupt magic or any differing field is refused before the dynamic
//! loader runs a single initializer. A library that passes is copied to a unique name (the
//! dynamic loader caches by path), opened, and its exported `regia_extension_stamp` and
//! `REGIA_ABI_HASH` are compared again; only then is `regia_extension_entry` resolved and called.
//! The library is never unloaded (thread-local destructors make `dlclose` unsafe).

#![allow(unsafe_code)] // `libloading` symbol lookup (docs/04-conventions.md § 5); see each SAFETY note

use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use libloading::Library;
use regia_framework::extension::RegiaExtension;
use regia_framework::extension::stamp::{
    ENTRY_SYMBOL, ExtensionStamp, STAMP_SIZE, STAMP_SYMBOL, StampError, manifest_hash,
};

use super::code_signature::check_code_signature;
use crate::hot_reload::{dylib_extension, host_abi_hash, process_links_dynamic_std};

/// How many times this process called an extension's entry symbol. The stamp refusal tests read
/// it to prove a refused library's entry never ran.
static ENTRY_CALLS: AtomicU64 = AtomicU64::new(0);
/// Copies made by this process. Part of every copy's name: a reload that failed keeps its
/// generation, and a retry under the same name would overwrite a library that is still mapped.
static COPIES: AtomicU64 = AtomicU64::new(0);

/// Why a library was refused. The library stays out of the process (or, after a late refusal,
/// mapped and unused); the editor continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionLoadError {
    /// No readable stamp: missing symbol, short read, bad magic (`not_an_extension`).
    NotAnExtension(String),
    /// A stamp field differs from this build (`abi_mismatch`).
    AbiMismatch {
        /// The first differing field.
        field: &'static str,
    },
    /// The file, the dynamic loader or the entry point failed (`load_failed`).
    LoadFailed(String),
}

impl ExtensionLoadError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAnExtension(_) => "not_an_extension",
            Self::AbiMismatch { .. } => "abi_mismatch",
            Self::LoadFailed(_) => "load_failed",
        }
    }
}

impl std::fmt::Display for ExtensionLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnExtension(detail) => write!(f, "not_an_extension: {detail}"),
            Self::AbiMismatch { field } => write!(
                f,
                "abi_mismatch: `{field}` differs from this engine build; rebuild with `regia plugin build`"
            ),
            Self::LoadFailed(detail) => write!(f, "load_failed: {detail}"),
        }
    }
}

impl std::error::Error for ExtensionLoadError {}

impl From<StampError> for ExtensionLoadError {
    fn from(error: StampError) -> Self {
        match error {
            StampError::NotAnExtension(detail) => Self::NotAnExtension(detail),
            StampError::Mismatch { field } => Self::AbiMismatch { field },
        }
    }
}

/// A dynamic extension in this process.
pub struct LoadedExtension {
    /// The extension.
    pub extension: Arc<dyn RegiaExtension>,
    /// Its stamp.
    pub stamp: ExtensionStamp,
    /// The unique copy that was loaded.
    pub library: PathBuf,
}

/// The stamp a library must carry to load here: this editor's ABI hash, the fingerprint of the
/// engine dylib in `target_dir`, and the hash of the manifest on disk.
#[must_use]
pub fn expected_stamp(manifest: &[u8], target_dir: &Path) -> ExtensionStamp {
    expected_stamp_for(manifest, crate::hot_reload::engine_fingerprint(target_dir))
}

/// [`expected_stamp`] with a fingerprint taken earlier. The host takes it once at start: the engine
/// on disk can be rebuilt while this process keeps the old one mapped, and a library linked against
/// the new one must not match.
#[must_use]
pub fn expected_stamp_for(manifest: &[u8], fingerprint: u64) -> ExtensionStamp {
    ExtensionStamp::for_build(host_abi_hash(), fingerprint, manifest_hash(manifest))
}

/// How many times this process called an extension's entry symbol.
#[must_use]
pub fn entry_calls() -> u64 {
    ENTRY_CALLS.load(Ordering::Relaxed)
}

/// The stamp check that needs no loading: finds the record in the file and compares it.
///
/// # Errors
///
/// `not_an_extension` when the file has no complete record; `abi_mismatch` naming the first
/// differing field of any record; `load_failed` when the file cannot be read or this host has no
/// engine fingerprint to compare (0 would match a library built without one).
pub fn check_library_file(
    path: &Path,
    expected: &ExtensionStamp,
) -> Result<ExtensionStamp, ExtensionLoadError> {
    if expected.fingerprint() == 0 {
        return Err(ExtensionLoadError::LoadFailed(
            "no engine library was found to fingerprint, so no extension can be proven to match".into(),
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| ExtensionLoadError::LoadFailed(format!("{}: {error}", path.display())))?;
    // The record is read from the exported `REGIA_EXTENSION_STAMP` symbol, never found by position
    // (ADR-2844); the loader compares the record the opened copy returns once more.
    let record = ExtensionStamp::from_object_bytes(&bytes)?;
    record.check_against(expected)?;
    Ok(record)
}

/// The verdict of [`host_links_dynamic_std`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStd {
    /// The running executable asks the loader for the dynamic `std`.
    Dynamic,
    /// It does not: a dynamic extension would get a second, private `std`.
    Static,
    /// The executable could not be read as an object, so nothing is proven either way.
    Unknown,
}

/// Whether this process links Rust's dynamic `std`, on every platform. Linux asks the loader's
/// own map (`/proc/self/maps`); Windows and macOS read the executable's file bytes with the
/// bounds-checked object readers and look for `std-*.dll` or `libstd-*.dylib` among the libraries
/// it asks for (ADR-2844).
#[must_use]
pub fn host_links_dynamic_std() -> HostStd {
    if cfg!(target_os = "linux") {
        return if process_links_dynamic_std() {
            HostStd::Dynamic
        } else {
            HostStd::Static
        };
    }
    let Ok(exe) = std::env::current_exe() else {
        return HostStd::Unknown;
    };
    let Ok(bytes) = std::fs::read(&exe) else {
        return HostStd::Unknown;
    };
    std_verdict(&bytes)
}

/// [`host_links_dynamic_std`] for an executable's file bytes.
#[must_use]
pub fn std_verdict(executable: &[u8]) -> HostStd {
    match regia_framework::extension::objfile::links_dynamic_std(executable) {
        Ok(true) => HostStd::Dynamic,
        Ok(false) => HostStd::Static,
        Err(error) => {
            tracing::warn!(%error, "could not read this executable to check for the dynamic std");
            HostStd::Unknown
        }
    }
}

const STATIC_STD_MESSAGE: &str = "this editor is statically linked to Rust std, so a dynamic extension would get a second copy; rebuild the editor with the hot-dylib feature and `-C prefer-dynamic` (the executable must ask the loader for libstd-*.so, std-*.dll or libstd-*.dylib) and put the toolchain's library directory on the loader path";

/// Whether a dynamic-loader failure means a dependency is absent, not a bad file.
fn missing_dependency(detail: &str) -> bool {
    [
        "cannot open shared object",
        "could not be found",
        "Library not loaded",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

/// Reads the exported stamp bytes without trusting them.
fn exported_stamp(library: &Library) -> Result<ExtensionStamp, ExtensionLoadError> {
    // SAFETY: `regia::extension!` defines the symbol as `extern "C" fn() -> *const ExtensionStamp`.
    // The file already carried a stamp matching this build, so it was built by that macro. The
    // function returns the address of a `static` and allocates nothing. The caller leaks the
    // library, so the static stays mapped while it is read.
    let pointer = unsafe {
        let symbol = library
            .get::<unsafe extern "C" fn() -> *const u8>(STAMP_SYMBOL.as_bytes())
            .map_err(|error| ExtensionLoadError::NotAnExtension(format!("{STAMP_SYMBOL}: {error}")))?;
        symbol()
    };
    // SAFETY: the pointer addresses a `#[repr(C)]` `ExtensionStamp` of `STAMP_SIZE` bytes without
    // padding (a compile-time check in `stamp.rs`), so every byte is initialized.
    unsafe { copy_stamp(pointer) }
}

/// Copies the stamp at `pointer` out of a library and reads it; null is `not_an_extension`.
///
/// # Safety
///
/// `pointer` is null, or points to `STAMP_SIZE` readable, initialized bytes. It need not be aligned,
/// and none of the bytes are trusted.
unsafe fn copy_stamp(pointer: *const u8) -> Result<ExtensionStamp, ExtensionLoadError> {
    if pointer.is_null() {
        return Err(ExtensionLoadError::NotAnExtension(format!(
            "{STAMP_SYMBOL} returned null"
        )));
    }
    let mut bytes = [0u8; STAMP_SIZE];
    // SAFETY: the caller's contract; the copy is bytewise, so alignment does not matter.
    unsafe { std::ptr::copy_nonoverlapping(pointer, bytes.as_mut_ptr(), STAMP_SIZE) };
    Ok(ExtensionStamp::from_bytes(&bytes)?)
}

/// Calls the entry symbol. Only reached after every stamp field matched.
fn call_entry(library: &Library) -> Result<Box<dyn RegiaExtension>, ExtensionLoadError> {
    // SAFETY: the stamp proved this library was built by the same compiler, for the same target
    // and profile, against this engine build, so `Box<dyn RegiaExtension>` has one layout on both
    // sides and both link one allocator (`regia_dylib`, `-C prefer-dynamic`). `regia::extension!`
    // defines the symbol as `extern "C" fn() -> *mut c_void`, returning what `take_extension` wants,
    // once per call.
    unsafe {
        let symbol = library
            .get::<unsafe extern "C" fn() -> *mut std::ffi::c_void>(ENTRY_SYMBOL.as_bytes())
            .map_err(|error| ExtensionLoadError::NotAnExtension(format!("{ENTRY_SYMBOL}: {error}")))?;
        ENTRY_CALLS.fetch_add(1, Ordering::Relaxed);
        take_extension(symbol())
            .ok_or_else(|| ExtensionLoadError::LoadFailed(format!("{ENTRY_SYMBOL} panicked")))
    }
}

/// Takes back the extension the entry symbol released; `None` for the null of a caught panic.
///
/// # Safety
///
/// `raw` is null, or came from `Box::into_raw(Box::new(extension))` with an `extension:
/// Box<dyn RegiaExtension>` in this process's allocator and layout, and is taken back once.
unsafe fn take_extension(raw: *mut std::ffi::c_void) -> Option<Box<dyn RegiaExtension>> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: the caller's contract; the thin pointer is the outer box, the inner box stays whole.
    Some(*unsafe { Box::from_raw(raw.cast::<Box<dyn RegiaExtension>>()) })
}

fn open(copy: &Path) -> Result<ManuallyDrop<Library>, ExtensionLoadError> {
    crate::hot_reload::open_library(copy).map_err(|error| {
        let detail = error.to_string();
        if missing_dependency(&detail) {
            ExtensionLoadError::LoadFailed(detail)
        } else {
            ExtensionLoadError::NotAnExtension(detail)
        }
    })
}

/// A file name no earlier load of this process used.
fn copy_name(id: &str, generation: u64) -> String {
    format!(
        "{id}-{}-{generation}-{}.{}",
        std::process::id(),
        COPIES.fetch_add(1, Ordering::Relaxed),
        dylib_extension()
    )
}

/// Copies `path` to `copy` and scans the copy's stamp records again: the file the loader opens is
/// the one that was checked, even when `path` was replaced between the first scan and the copy
/// (audit wave 5, netplug L1). Then the copy's Mach-O code-signature pages are compared
/// ([`check_code_signature`]): on Apple silicon the kernel kills a process that maps a changed
/// signed page, so a damaged file must be refused here, not by the loader (ADR-3373). A copy that
/// fails either check is deleted.
fn copy_and_recheck(path: &Path, copy: &Path, expected: &ExtensionStamp) -> Result<(), ExtensionLoadError> {
    std::fs::copy(path, copy)
        .map_err(|error| ExtensionLoadError::LoadFailed(format!("{}: {error}", copy.display())))?;
    let checked = check_library_file(copy, expected).and_then(|_| {
        let bytes = std::fs::read(copy)
            .map_err(|error| ExtensionLoadError::LoadFailed(format!("{}: {error}", copy.display())))?;
        check_code_signature(&bytes)
            .map(|_| ())
            .map_err(|detail| ExtensionLoadError::NotAnExtension(format!("{}: {detail}", path.display())))
    });
    if let Err(error) = checked {
        let _ = std::fs::remove_file(copy);
        return Err(error);
    }
    Ok(())
}

/// Loads the library at `path` for extension `id`, as copy `generation` under `hot_dir`.
///
/// # Errors
///
/// [`ExtensionLoadError`]. Everything up to the copy happens without loading anything.
pub fn load_extension(
    path: &Path,
    expected: &ExtensionStamp,
    hot_dir: &Path,
    id: &str,
    generation: u64,
) -> Result<LoadedExtension, ExtensionLoadError> {
    let _span = tracing::info_span!("extension_load_dynamic", id).entered();
    check_library_file(path, expected)?;
    if cfg!(panic = "abort") {
        return Err(ExtensionLoadError::LoadFailed(
            "this editor was built with panic=abort, so a panic in an extension would end it".into(),
        ));
    }
    if host_links_dynamic_std() == HostStd::Static {
        return Err(ExtensionLoadError::LoadFailed(STATIC_STD_MESSAGE.into()));
    }
    std::fs::create_dir_all(hot_dir)
        .map_err(|error| ExtensionLoadError::LoadFailed(format!("{}: {error}", hot_dir.display())))?;
    let copy = hot_dir.join(copy_name(id, generation));
    copy_and_recheck(path, &copy, expected)?;
    let library = open(&copy)?;
    let stamp = exported_stamp(&library)?;
    stamp.check_against(expected)?;
    let abi_hash = crate::hot_reload::read_abi_hash(&library, &copy.display().to_string())
        .map_err(|error| ExtensionLoadError::NotAnExtension(error.to_string()))?;
    if abi_hash != expected.abi_hash {
        return Err(ExtensionLoadError::AbiMismatch { field: "abi_hash" });
    }
    let extension = call_entry(&library)?;
    if extension.id() != id {
        return Err(ExtensionLoadError::LoadFailed(format!(
            "the library's extension is `{}`, the manifest says `{id}`",
            extension.id()
        )));
    }
    // `ManuallyDrop` skips `dlclose`; the mapping lives for the process (specs/20 § 3).
    let _leaked = library;
    tracing::info!(id, library = %copy.display(), generation, "loaded dynamic extension");
    Ok(LoadedExtension {
        extension: Arc::from(extension),
        stamp,
        library: copy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Takes back exactly what `regia::extension!`'s entry releases, and the box's value survives;
    /// a caught panic's null is no extension. Runs under Miri.
    #[test]
    fn the_entry_box_round_trips_and_null_is_none() {
        struct Probe;
        impl RegiaExtension for Probe {
            fn id(&self) -> &str {
                "probe"
            }
            fn build(
                &self,
                _: &mut regia_framework::extension::ExtensionContext<'_>,
            ) -> Result<(), regia_framework::extension::ExtensionError> {
                Ok(())
            }
        }
        let extension: Box<dyn RegiaExtension> = Box::new(Probe);
        let raw = Box::into_raw(Box::new(extension)).cast::<std::ffi::c_void>();
        // SAFETY: `raw` is what the contract asks for, taken back once.
        let back = unsafe { take_extension(raw) }.expect("the extension");
        assert_eq!(back.id(), "probe");
        // SAFETY: null is allowed by the contract.
        assert!(unsafe { take_extension(std::ptr::null_mut()) }.is_none());
    }

    /// The stamp copy reads exactly `STAMP_SIZE` bytes at any alignment, and null and a corrupt magic
    /// are refused. Under Miri, one byte more would be an out-of-bounds read. Runs under Miri.
    #[test]
    fn the_stamp_copy_needs_no_alignment_and_refuses_null_and_junk() {
        let stamp = ExtensionStamp::for_build(1, 2, [3; 32]);
        for pad in 0..8 {
            let mut bytes = vec![0xAAu8; pad];
            bytes.extend_from_slice(&stamp.to_bytes());
            // SAFETY: `STAMP_SIZE` initialized bytes after the pad, and the vector ends there.
            let copied = unsafe { copy_stamp(bytes[pad..].as_ptr()) }.expect("a good stamp");
            assert_eq!(copied, stamp, "pad {pad}");
        }
        // SAFETY: null is allowed by the contract.
        let null = unsafe { copy_stamp(std::ptr::null()) }.expect_err("null");
        assert_eq!(null.code(), "not_an_extension");
        let mut junk = stamp.to_bytes();
        junk[0] ^= 0x41;
        // SAFETY: `STAMP_SIZE` initialized bytes.
        let refused = unsafe { copy_stamp(junk.as_ptr()) }.expect_err("a corrupt magic");
        assert_eq!(refused.code(), "not_an_extension");
    }

    #[test]
    fn a_host_with_no_engine_fingerprint_loads_nothing() {
        // 0 is also what a library built without `REGIA_ENGINE_FINGERPRINT` carries.
        let expected = ExtensionStamp::for_build(1, 0, [3; 32]);
        let (dir, file) = fake_library("ext-no-fingerprint", &expected);
        let error = check_library_file(&file, &expected).expect_err("refused");
        assert_eq!(error.code(), "load_failed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit wave 5 (netplug L1): the copy the loader opens is scanned itself. A source whose
    /// stamp no longer matches by the time it is copied is refused and its copy deleted; a
    /// matching one is kept.
    #[test]
    fn the_copy_is_scanned_before_the_loader_opens_it() {
        let expected = ExtensionStamp::for_build(1, 2, [3; 32]);
        let other = ExtensionStamp::for_build(1, 2, [4; 32]);
        let (dir, swapped) = fake_library("ext-copy-recheck", &other);
        let copy = dir.join("hot-copy.so");
        let error = copy_and_recheck(&swapped, &copy, &expected).expect_err("refused");
        assert_eq!(error.code(), "abi_mismatch");
        assert!(!copy.exists(), "the refused copy is gone");
        let (dir2, good) = fake_library("ext-copy-recheck-ok", &expected);
        copy_and_recheck(&good, &copy, &expected).expect("kept");
        assert!(copy.exists());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// ADR-3373: a signed Mach-O copy with one changed byte outside the stamp passes the stamp
    /// scan but is refused before the loader opens it (on Apple silicon the kernel would kill the
    /// process), and the copy is deleted; the intact library is kept.
    #[test]
    fn a_changed_signed_page_is_refused_before_the_loader() {
        // The stamp is the exported `REGIA_EXTENSION_STAMP` symbol (ADR-2844): a record placed
        // in the body by position is no stamp, so the fixture exports it (integfix-misc-reds).
        use super::super::code_signature::tests::signed_stamped_dylib;
        let expected = ExtensionStamp::for_build(1, 2, [3; 32]);
        let image = signed_stamped_dylib(&[0x5a; 6_000], &expected.to_bytes());
        let dir = regia_test_utils::unique_temp_dir("ext-codesign");
        let good = dir.join("libgood.dylib");
        std::fs::write(&good, &image).expect("write");
        let copy = dir.join("hot-copy.dylib");
        copy_and_recheck(&good, &copy, &expected).expect("an intact signed library is kept");
        assert!(copy.exists());
        let mut bytes = image;
        bytes[100] ^= 0x41;
        let damaged = dir.join("libdamaged.dylib");
        std::fs::write(&damaged, &bytes).expect("write");
        check_library_file(&damaged, &expected).expect("the stamp itself is intact");
        let copy2 = dir.join("hot-copy2.dylib");
        let error = copy_and_recheck(&damaged, &copy2, &expected).expect_err("refused");
        assert_eq!(error.code(), "not_an_extension", "{error}");
        assert!(error.to_string().contains("page 0"), "{error}");
        assert!(!copy2.exists(), "the refused copy is gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_retry_at_the_same_generation_gets_its_own_file() {
        assert_ne!(copy_name("dialogue", 3), copy_name("dialogue", 3));
    }
    use regia_framework::extension::stamp::STAMP_FIELDS;

    fn fake_library(tag: &str, stamp: &ExtensionStamp) -> (PathBuf, PathBuf) {
        let dir = regia_test_utils::unique_temp_dir(tag);
        let file = dir.join("libfixture.so");
        let bytes = regia_framework::extension::objfile::synth::elf64(
            &[(
                regia_framework::extension::stamp::STAMP_DATA_SYMBOL,
                &stamp.to_bytes(),
            )],
            &[],
        );
        std::fs::write(&file, bytes).expect("write");
        (dir, file)
    }

    /// ADR-2844 / audit H3: a record anywhere but the exported symbol neither passes nor fails a file.
    #[test]
    fn only_the_exported_symbol_counts_not_a_record_by_position() {
        use regia_framework::extension::objfile::synth;
        use regia_framework::extension::stamp::STAMP_DATA_SYMBOL;
        let expected = ExtensionStamp::for_build(1, 2, [3; 32]);
        let other = ExtensionStamp::for_build(1, 2, [4; 32]);
        let dir = regia_test_utils::unique_temp_dir("ext-position");
        // A good decoy record in front of the object, a bad exported one: refused.
        let mut bad = expected.to_bytes().to_vec();
        bad.extend_from_slice(&synth::elf64(&[(STAMP_DATA_SYMBOL, &other.to_bytes())], &[]));
        // The reverse: a bad record planted at the end, the exported one good: accepted.
        let mut good = synth::elf64(&[(STAMP_DATA_SYMBOL, &expected.to_bytes())], &[]);
        good.extend_from_slice(&other.to_bytes());
        // Records but no exported symbol, and a raw record with no object around it.
        let no_symbol = synth::elf64(&[("something_else", &expected.to_bytes())], &["libc.so.6"]);
        for (name, bytes, want) in [
            ("bad.so", bad.clone(), "not_an_extension"),
            ("good.so", good, ""),
            ("nosym.so", no_symbol, "not_an_extension"),
            ("raw.so", expected.to_bytes().to_vec(), "not_an_extension"),
        ] {
            let file = dir.join(name);
            std::fs::write(&file, &bytes).expect("write");
            match check_library_file(&file, &expected) {
                Ok(_) => assert!(want.is_empty(), "{name} passed"),
                Err(error) => assert_eq!(error.code(), want, "{name}: {error}"),
            }
        }
        // `bad.so` leads with a matching record yet the exported one differs: a mismatch, not a pass.
        let file = dir.join("bad.so");
        // (The object follows 232 bytes of decoy, so it is not an ELF file at offset 0 either.)
        assert!(check_library_file(&file, &expected).is_err());
        let mut elf_first = synth::elf64(&[(STAMP_DATA_SYMBOL, &other.to_bytes())], &[]);
        elf_first.extend_from_slice(&expected.to_bytes());
        let file = dir.join("elf-first.so");
        std::fs::write(&file, elf_first).expect("write");
        assert_eq!(
            check_library_file(&file, &expected).expect_err("refused"),
            ExtensionLoadError::AbiMismatch {
                field: "manifest_hash"
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_std_verdict_reads_each_object_format() {
        use regia_framework::extension::objfile::synth;
        let dynamic = [
            synth::elf64(&[], &["libc.so.6", "libstd-0123abcd.so"]),
            synth::pe64(&[], &["KERNEL32.dll", "std-0123abcd.dll"]),
            synth::macho64(
                &[],
                &["/usr/lib/libSystem.B.dylib", "@rpath/libstd-0123abcd.dylib"],
            ),
        ];
        let statics = [
            synth::elf64(&[], &["libc.so.6"]),
            synth::pe64(&[], &["KERNEL32.dll"]),
            synth::macho64(&[], &["/usr/lib/libSystem.B.dylib"]),
        ];
        for bytes in &dynamic {
            assert_eq!(std_verdict(bytes), HostStd::Dynamic);
        }
        for bytes in &statics {
            assert_eq!(std_verdict(bytes), HostStd::Static);
        }
        // Not an object, or cut short: nothing is proven, nothing is refused.
        assert_eq!(std_verdict(b"not an executable"), HostStd::Unknown);
        assert_eq!(std_verdict(&dynamic[1][..300]), HostStd::Unknown);
        assert!(STATIC_STD_MESSAGE.contains("prefer-dynamic") && STATIC_STD_MESSAGE.contains("std-*.dll"));
    }

    #[test]
    fn a_file_with_no_stamp_is_not_an_extension() {
        let expected = ExtensionStamp::for_build(1, 2, [3; 32]);
        let dir = regia_test_utils::unique_temp_dir("ext-nostamp");
        let file = dir.join("libnothing.so");
        std::fs::write(&file, b"nothing here").expect("write");
        let calls = entry_calls();
        let error = load_extension(&file, &expected, &dir.join("hot"), "x", 0)
            .err()
            .expect("refused");
        assert_eq!(error.code(), "not_an_extension");
        assert_eq!(entry_calls(), calls, "no entry was called");
        assert!(!dir.join("hot").exists(), "nothing was copied");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_differing_field_is_named_before_anything_loads() {
        let expected = ExtensionStamp::for_build(1, 2, [3; 32]);
        let (dir, file) = fake_library("ext-fields", &expected);
        let offset = std::fs::read(&file)
            .expect("read")
            .windows(8)
            .position(|w| w == b"REGIAEXT")
            .expect("stamp");
        let calls = entry_calls();
        for (field, at, _) in STAMP_FIELDS.iter().skip(1) {
            let mut bytes = std::fs::read(&file).expect("read");
            bytes[offset + at] ^= 0x41;
            let copy = dir.join(format!("{field}.so"));
            std::fs::write(&copy, bytes).expect("write");
            let error = load_extension(&copy, &expected, &dir.join("hot"), "x", 0)
                .err()
                .expect("refused");
            assert_eq!(error, ExtensionLoadError::AbiMismatch { field }, "{field}");
        }
        let mut bytes = std::fs::read(&file).expect("read");
        bytes[offset] ^= 0x41;
        let copy = dir.join("magic.so");
        std::fs::write(&copy, bytes).expect("write");
        let error = load_extension(&copy, &expected, &dir.join("hot"), "x", 0)
            .err()
            .expect("refused");
        assert_eq!(error.code(), "not_an_extension");
        assert_eq!(entry_calls(), calls, "no entry was called");
        assert!(!dir.join("hot").exists(), "nothing was copied or opened");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_file_is_not_an_extension() {
        let expected = ExtensionStamp::for_build(1, 2, [3; 32]);
        let (dir, file) = fake_library("ext-short", &expected);
        let bytes = std::fs::read(&file).expect("read");
        std::fs::write(&file, &bytes[..bytes.len() - 200]).expect("write");
        let error = check_library_file(&file, &expected).expect_err("refused");
        assert_eq!(error.code(), "not_an_extension");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_a_load_failure() {
        let expected = ExtensionStamp::for_build(1, 2, [3; 32]);
        let dir = regia_test_utils::unique_temp_dir("ext-missing");
        let error = check_library_file(&dir.join("nope.so"), &expected).expect_err("refused");
        assert_eq!(error.code(), "load_failed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_dependency_wording_is_recognized() {
        assert!(missing_dependency(
            "libregia_dylib.so: cannot open shared object file"
        ));
        assert!(missing_dependency("The specified module could not be found."));
        assert!(!missing_dependency("file too short"));
        assert!(!missing_dependency("invalid ELF header"));
    }
}
