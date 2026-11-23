// SPDX-License-Identifier: MPL-2.0
//! Load a game dylib, check its ABI, never unload it.
//!
//! Specification: `docs/specs/20-hot-reload.md` §§ 2–3 and ADR-015.

#![allow(unsafe_code)] // `libloading` symbol lookup (docs/04-conventions.md § 5); see each SAFETY note

use std::fmt;
use std::mem::ManuallyDrop;
use std::path::Path;

use libloading::Library;

/// Name of the `u64` static `export_game!` emits (specs/20 § 2).
const ABI_HASH_SYMBOL: &[u8] = b"REGIA_ABI_HASH";
/// Name of the entry function `export_game!` emits (specs/20 § 2).
const GAME_ENTRY_SYMBOL: &[u8] = b"regia_game_entry";

/// Why a game dylib could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameLibraryError {
    /// `path` does not name a file.
    MissingFile {
        /// Path that was requested.
        path: String,
    },
    /// The dynamic loader could not open the file.
    LoadFailed {
        /// Path that was requested.
        path: String,
        /// Loader detail (`dlopen` / `LoadLibrary`).
        detail: String,
    },
    /// `REGIA_ABI_HASH` or `regia_game_entry` is not in the library.
    MissingSymbol {
        /// Path that was opened.
        path: String,
        /// Symbol that was missing.
        symbol: &'static str,
        /// Loader detail.
        detail: String,
    },
    /// The library's `REGIA_ABI_HASH` is not the hash the editor was given.
    AbiMismatch {
        /// Path that was opened.
        path: String,
        /// Hash read from the library.
        found: u64,
        /// Hash the caller expected.
        expected: u64,
    },
    /// `regia_game_entry` returned a null export table, or a table with a null function
    /// pointer in it.
    NullEntry {
        /// Path that was opened.
        path: String,
    },
    /// This process is linked to a private copy of Rust `std`.
    ///
    /// A game dylib allocates with the process allocator. Two copies corrupt the heap, so the
    /// editor refuses the load. The linkage is probed per OS: `/proc/self/maps` on Linux,
    /// loaded-module enumeration on Windows, dyld's image list on macOS; other targets skip
    /// the check (the editor compiles for Android but never runs there). Rebuild the editor
    /// with `-C prefer-dynamic` and put the toolchain's `lib/rustlib/<target>/lib` on
    /// `LD_LIBRARY_PATH` (Linux), `PATH` (Windows) or `DYLD_LIBRARY_PATH` (macOS).
    StaticStd,
}

impl fmt::Display for GameLibraryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFile { path } => write!(formatter, "game library file is missing: {path}"),
            Self::LoadFailed { path, detail } => {
                write!(formatter, "cannot load game library {path}: {detail}")
            }
            Self::MissingSymbol { path, symbol, detail } => {
                write!(
                    formatter,
                    "game library {path} is missing symbol {symbol}: {detail}"
                )
            }
            Self::AbiMismatch {
                path,
                found,
                expected,
            } => write!(
                formatter,
                "game library {path} ABI hash {found:016x} does not match expected {expected:016x}; rebuild the editor"
            ),
            Self::NullEntry { path } => {
                write!(formatter, "game library {path} returned a null regia_game_entry")
            }
            Self::StaticStd => write!(
                formatter,
                "editor is statically linked to Rust std; rebuild it with -C prefer-dynamic and put the toolchain lib dir on the dynamic loader path"
            ),
        }
    }
}

impl std::error::Error for GameLibraryError {}

/// `register` / `on_unload` / `on_load` copied out of a game dylib: the `#[repr(C)]` table
/// `regia_framework::export_game!` generates (one definition, shared by both sides).
pub use regia_framework::GameExports;

/// A game dylib mapped into this process.
///
/// Dropping this value does not unload the library. Rust dylibs register TLS destructors, so
/// `dlclose` is unsafe (specs/20 § 3). Each load leaks that mapping.
#[derive(Debug, Clone, Copy)]
pub struct LoadedGameLibrary {
    abi_hash: u64,
    entry_address: usize,
}

impl LoadedGameLibrary {
    /// Value of the `REGIA_ABI_HASH` static.
    #[must_use]
    pub fn abi_hash(self) -> u64 {
        self.abi_hash
    }

    /// Address of `regia_game_entry`.
    ///
    /// The library stays mapped, so this address remains valid for the process.
    #[must_use]
    pub fn entry_address(self) -> usize {
        self.entry_address
    }
}

/// Decimal `REGIA_ABI_HASH` baked into this editor build (specs/20 § 2).
#[must_use]
pub fn host_abi_hash() -> u64 {
    parse_abi_hash(env!("REGIA_ABI_HASH"))
}

pub(crate) fn parse_abi_hash(text: &str) -> u64 {
    // `wrapping_mul` wraps before `u64::MAX`. A wide accumulator keeps the decimal the build
    // script printed (`REGIA_ABI_HASH=18446744073709551615` must round-trip).
    let mut value = 0u128;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return 0;
        }
        value = value * 10 + u128::from(byte - b'0');
        if value > u128::from(u64::MAX) {
            return 0;
        }
    }
    u64::try_from(value).unwrap_or(0)
}

/// Dynamic library extension for the running OS (`so`, `dll`, `dylib`).
#[must_use]
pub fn dylib_extension() -> &'static str {
    if cfg!(windows) {
        "dll"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

/// True when this process already maps Rust's dynamic `libstd`.
///
/// The game build always passes `-C prefer-dynamic`, so a game dylib imports the toolchain's
/// `std` shared library. Loading it into a process whose own `std` is statically linked maps
/// a second `std` (a second allocator) and the two corrupt the heap, so the loaders refuse the
/// dylib when this returns `false` ([`GameLibraryError::StaticStd`]). The probe is platform
/// specific:
///
/// - Linux: any mapping in `/proc/self/maps` whose file is `libstd-*.so`.
/// - Windows: any loaded module named `std-*.dll`.
/// - macOS: any dyld image named `libstd-*.dylib`.
/// - Other targets: the check is skipped and this returns `true`.
///
/// On Windows and macOS a `std` shared library is mapped iff the editor itself was built with
/// `-C prefer-dynamic` (the check runs before the game dylib can pull one in), so the module
/// list answers the same question `/proc/self/maps` does on Linux. A failed probe (unreadable
/// maps, module list unavailable) reports "not linked" and the load is refused, matching
/// Linux's fail-closed behavior.
#[must_use]
pub fn process_links_dynamic_std() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/maps")
            .map(|maps| maps_report_dynamic_std(&maps))
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        process_modules_include_dynamic_std()
    }
    #[cfg(target_os = "macos")]
    {
        dyld_images_include_dynamic_std()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        true
    }
}

/// True when `file_name` is a Rust toolchain `std` shared library: `std-<hash>.dll` on
/// Windows, `libstd-<hash>.so` on Linux, `libstd-<hash>.dylib` on macOS. The Windows import
/// library (`std-<hash>.dll.lib`) does not match: only a loaded copy proves the dynamic `std`
/// is already mapped. Compiled on the probing targets and in tests only (other targets never
/// call it).
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos", test))]
fn is_dynamic_std(file_name: &str) -> bool {
    let std_prefix = file_name.starts_with("std-") || file_name.starts_with("libstd-");
    std_prefix
        && matches!(
            file_name.rsplit_once('.').map(|(_, ext)| ext),
            Some("dll" | "so" | "dylib")
        )
}

/// Linux probe over `/proc/self/maps` text: any mapped file is a dynamic `libstd`. Lines
/// without a path field (anonymous mappings) are ignored; only the file name is matched, so a
/// `libstd-` substring in a directory name does not count. A mapped-but-unlinked file's line
/// ends in ` (deleted)` (the Rust toolchain replaced under a running prefer-dynamic editor):
/// the suffix is stripped so the deleted-but-mapped std still counts.
#[cfg(any(target_os = "linux", test))]
fn maps_report_dynamic_std(maps: &str) -> bool {
    maps.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let Some(mut field) = fields.next_back() else {
            return false;
        };
        if field == "(deleted)" {
            field = match fields.next_back() {
                Some(previous) => previous,
                None => return false,
            };
        }
        if !field.contains('/') {
            return false;
        }
        is_dynamic_std(field.rsplit('/').next().unwrap_or(field))
    })
}

/// Windows probe: true when any module loaded in this process is the toolchain's `std-*.dll`
/// (enumerated with the kernel32 entry points; the regression test repeats the enumeration
/// through the psapi entry points as an independent probe).
#[cfg(target_os = "windows")]
fn process_modules_include_dynamic_std() -> bool {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::ProcessStatus::{K32EnumProcessModules, K32GetModuleBaseNameW};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: `GetCurrentProcess` returns the current-process pseudo handle: it needs no
    // cleanup and is valid for both kernel32 calls below.
    let process = unsafe { GetCurrentProcess() };
    // Each hot reload permanently maps another game DLL, so a long session can pass any
    // fixed module count; grow to `bytes_needed` and re-enumerate instead of scanning a
    // truncated prefix (audit G editor2-platform L4).
    let module_size = std::mem::size_of::<HMODULE>();
    let mut modules: Vec<HMODULE> = vec![std::ptr::null_mut(); 1024];
    let mut bytes_needed: u32 = 0;
    loop {
        // SAFETY: `modules` is writable for `modules.len()` handles for the call's duration
        // and `bytes_needed` is a valid `u32` out-param.
        let listed = unsafe {
            K32EnumProcessModules(
                process,
                modules.as_mut_ptr(),
                (modules.len() * module_size) as u32,
                &mut bytes_needed,
            )
        };
        if listed == 0 {
            // Fail closed: refuse the load rather than risk a second `std`.
            return false;
        }
        let needed = usize::try_from(bytes_needed).unwrap_or(0);
        if needed <= modules.len() * module_size {
            break;
        }
        modules.resize(needed / module_size + 1, std::ptr::null_mut());
    }
    let count = usize::try_from(bytes_needed).unwrap_or(0) / module_size;
    let mut wide = [0u16; 512];
    for module in modules.iter().take(count.min(modules.len())) {
        // SAFETY: `wide` is writable for 512 UTF-16 code units for the call's duration.
        let len = unsafe { K32GetModuleBaseNameW(process, *module, wide.as_mut_ptr(), wide.len() as u32) };
        if len == 0 {
            continue;
        }
        let end = usize::try_from(len).unwrap_or(0).min(wide.len());
        if is_dynamic_std(&String::from_utf16_lossy(&wide[..end])) {
            return true;
        }
    }
    false
}

/// macOS probe: true when dyld's image list holds the toolchain's `libstd-*.dylib`. The
/// images come from the public `mach-o/dyld.h` API, linked into every process by dyld.
#[cfg(target_os = "macos")]
fn dyld_images_include_dynamic_std() -> bool {
    // SAFETY: (the block) both functions are dyld's public `mach-o/dyld.h` API — the header
    // declares `_dyld_image_count` returning and `_dyld_get_image_name` taking `uint32_t` —
    // linked into every process by dyld; declaring them introduces no user code.
    unsafe extern "C" {
        fn _dyld_image_count() -> u32;
        fn _dyld_get_image_name(index: u32) -> *const std::ffi::c_char;
    }
    // SAFETY: both functions are dyld's public API; the image list lives for the process and
    // each returned pointer is a nul-terminated string owned by dyld. dyld never unloads
    // images from a running process and Regia never `dlclose`s its game or extension dylibs
    // (specs/20 § 3), so a returned name pointer cannot dangle; a concurrent `dlopen` only
    // appends, and an index past the list end returns null (skipped below).
    let count = unsafe { _dyld_image_count() };
    for index in 0..count {
        // SAFETY: `index` is below the live image count the API just reported.
        let name = unsafe { _dyld_get_image_name(index) };
        if name.is_null() {
            continue;
        }
        // SAFETY: `name` is the nul-terminated path dyld returned.
        let bytes = unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes();
        let file_name = bytes.rsplit(|&byte| byte == b'/').next().unwrap_or(bytes);
        let Ok(file_name) = std::str::from_utf8(file_name) else {
            continue;
        };
        if is_dynamic_std(file_name) {
            return true;
        }
    }
    false
}

/// Loads `path`, reads `REGIA_ABI_HASH` and `regia_game_entry`, and checks the hash.
///
/// `expected_abi_hash` is the engine's hash for this editor build (rustc version + regia build
/// id + enabled features). A mismatch returns [`GameLibraryError::AbiMismatch`] and tells the
/// caller to rebuild the editor.
///
/// The library is never unloaded, including when symbol lookup or the hash check fails after a
/// successful open.
///
/// # Errors
///
/// - [`GameLibraryError::MissingFile`] if `path` is not a file.
/// - [`GameLibraryError::LoadFailed`] if the dynamic loader rejects the file.
/// - [`GameLibraryError::MissingSymbol`] if either symbol is absent.
/// - [`GameLibraryError::AbiMismatch`] if the static's value is not `expected_abi_hash`.
/// - [`GameLibraryError::StaticStd`] if this process has a private Rust `std`.
pub fn load_game_library(path: &Path, expected_abi_hash: u64) -> Result<LoadedGameLibrary, GameLibraryError> {
    if !process_links_dynamic_std() {
        return Err(GameLibraryError::StaticStd);
    }
    let library = open_library(path)?;
    let path_string = path.display().to_string();
    let loaded = read_game_symbols(&library, &path_string, expected_abi_hash);
    if let Ok(loaded) = loaded {
        tracing::info!(
            path = %path_string,
            abi_hash = loaded.abi_hash(),
            entry = loaded.entry_address(),
            "loaded game library"
        );
    }
    // `ManuallyDrop` skips `Library`'s destructor, so leaving it here leaks the mapping.
    let _leaked = library;
    loaded
}

/// Loads `path` and copies the game's export table.
///
/// The library is leaked on success and on failure after a successful open.
///
/// # Errors
///
/// Same as [`load_game_library`], plus [`GameLibraryError::NullEntry`].
pub fn load_game_exports(path: &Path, expected_abi_hash: u64) -> Result<GameExports, GameLibraryError> {
    if !process_links_dynamic_std() {
        return Err(GameLibraryError::StaticStd);
    }
    let library = open_library(path)?;
    let path_string = path.display().to_string();
    let loaded = read_exports(&library, &path_string, expected_abi_hash);
    // `ManuallyDrop` skips `Library`'s destructor, so leaving it here leaks the mapping.
    let _leaked = library;
    loaded
}

pub(crate) fn open_library(path: &Path) -> Result<ManuallyDrop<Library>, GameLibraryError> {
    if !path.is_file() {
        return Err(GameLibraryError::MissingFile {
            path: path.display().to_string(),
        });
    }
    let path_string = path.display().to_string();
    // SAFETY: opening runs the library's initializers, whatever the file is. Callers pass a game or
    // extension library the host's own workflow built (specs/20 § 1), trusted as much as the host's
    // executable; the ABI hash and stamp are checked after this and before any entry is called, and
    // catch a stale build, not a hostile one. `ManuallyDrop` keeps `Library`'s destructor (`dlclose`)
    // from ever running, so every address read from the library stays mapped.
    let library = unsafe {
        open_eager(path).map_err(|err| GameLibraryError::LoadFailed {
            path: path_string,
            detail: err.to_string(),
        })?
    };
    Ok(ManuallyDrop::new(library))
}

/// `dlopen` flags for the game library: bind every import now (`RTLD_NOW`) so the first frame after
/// a reload does not pay lazy symbol resolution on worker threads, and keep its symbols private.
#[cfg(unix)]
const OPEN_FLAGS: std::ffi::c_int = libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_LOCAL;

/// Opens the library with eager binding on Unix.
///
/// # Safety
/// Runs the library's initialisers, as `Library::new` does.
#[cfg(unix)]
unsafe fn open_eager(path: &Path) -> Result<Library, libloading::Error> {
    // SAFETY: forwarded to the caller.
    unsafe { libloading::os::unix::Library::open(Some(path), OPEN_FLAGS).map(Library::from) }
}

/// Opens the library; Windows `LoadLibrary` already binds imports at load.
///
/// # Safety
/// Runs the library's initialisers, as `Library::new` does.
#[cfg(not(unix))]
unsafe fn open_eager(path: &Path) -> Result<Library, libloading::Error> {
    // SAFETY: forwarded to the caller.
    unsafe { Library::new(path) }
}

fn reject_abi_mismatch(path: &str, found: u64, expected: u64) -> Result<(), GameLibraryError> {
    if found == expected {
        Ok(())
    } else {
        Err(GameLibraryError::AbiMismatch {
            path: path.to_string(),
            found,
            expected,
        })
    }
}

fn read_game_symbols(
    library: &Library,
    path: &str,
    expected_abi_hash: u64,
) -> Result<LoadedGameLibrary, GameLibraryError> {
    let abi_hash = read_abi_hash(library, path)?;
    let entry_address = read_entry_address(library, path)?;
    reject_abi_mismatch(path, abi_hash, expected_abi_hash)?;
    Ok(LoadedGameLibrary {
        abi_hash,
        entry_address,
    })
}

fn read_exports(
    library: &Library,
    path: &str,
    expected_abi_hash: u64,
) -> Result<GameExports, GameLibraryError> {
    let abi_hash = read_abi_hash(library, path)?;
    reject_abi_mismatch(path, abi_hash, expected_abi_hash)?;
    // SAFETY: `export_game!` / `hot_fixture` define `regia_game_entry` as
    // `extern "Rust" fn() -> *const GameExports`; calling it runs the game's own code, which the
    // editor trusts as much as the initializers `Library::new` already ran (specs/20 § 1). The
    // pointed-to table is a leaked `#[repr(C)]` static of three function pointers, so it is
    // readable for `size_of::<GameExports>()` bytes; `copy_export_table` copies it without
    // assuming alignment and refuses null slots. The caller leaks `library`, so the pointers
    // stay mapped.
    unsafe {
        let symbol = library
            .get::<unsafe extern "Rust" fn() -> *const u8>(GAME_ENTRY_SYMBOL)
            .map_err(|err| GameLibraryError::MissingSymbol {
                path: path.to_string(),
                symbol: "regia_game_entry",
                detail: err.to_string(),
            })?;
        let table = symbol();
        if table.is_null() {
            return Err(GameLibraryError::NullEntry {
                path: path.to_string(),
            });
        }
        copy_export_table(table, path)
    }
}

/// [`GameExports`] as it is read out of a library: the same `#[repr(C)]` layout, each slot
/// nullable (`Option` of a function pointer is guaranteed to be a function pointer with `None`
/// as null), so a null slot is an error instead of an invalid function pointer.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawGameExports {
    register: Option<extern "Rust" fn(&mut regia_app::prelude::App)>,
    on_unload: Option<extern "Rust" fn(&mut regia_app::prelude::World)>,
    on_load: Option<extern "Rust" fn(&mut regia_app::prelude::World)>,
}

const _: () = assert!(size_of::<RawGameExports>() == size_of::<GameExports>());

impl RawGameExports {
    /// The table with every slot filled, or [`GameLibraryError::NullEntry`].
    fn into_exports(self, path: &str) -> Result<GameExports, GameLibraryError> {
        match (self.register, self.on_unload, self.on_load) {
            (Some(register), Some(on_unload), Some(on_load)) => Ok(GameExports {
                register,
                on_unload,
                on_load,
            }),
            _ => Err(GameLibraryError::NullEntry {
                path: path.to_string(),
            }),
        }
    }
}

/// Copies the export table at `table` out of a loaded game library.
///
/// Regression (unsafe audit 2026-10-21): this was `ptr::read::<GameExports>`, which assumed the
/// address was aligned and every slot non-null; a library breaking either was undefined behavior
/// at the copy. Now a misaligned table copies correctly and a null slot is an error.
///
/// # Safety
///
/// `table` must point to `size_of::<GameExports>()` initialized, readable bytes (the static
/// `export_game!` emits). It need not be aligned.
unsafe fn copy_export_table(table: *const u8, path: &str) -> Result<GameExports, GameLibraryError> {
    // SAFETY: the caller guarantees the bytes are readable and initialized; `read_unaligned`
    // makes no alignment assumption, and any pointer-sized word is a valid `Option` of a
    // function pointer (null is `None`, rejected by `into_exports`).
    let raw = unsafe { std::ptr::read_unaligned(table.cast::<RawGameExports>()) };
    raw.into_exports(path)
}

pub(crate) fn read_abi_hash(library: &Library, path: &str) -> Result<u64, GameLibraryError> {
    // SAFETY: specs/20 § 2 defines `REGIA_ABI_HASH` as a `u64` static. libloading represents a
    // data symbol as `Symbol<*const u64>`: one dereference is the static's address. The caller
    // leaks the library, so the address stays mapped for the read in `copy_abi_hash`.
    unsafe {
        let symbol =
            library
                .get::<*const u64>(ABI_HASH_SYMBOL)
                .map_err(|err| GameLibraryError::MissingSymbol {
                    path: path.to_string(),
                    symbol: "REGIA_ABI_HASH",
                    detail: err.to_string(),
                })?;
        copy_abi_hash(*symbol, path)
    }
}

/// Copies the `u64` behind the `REGIA_ABI_HASH` symbol.
///
/// Regression (unsafe audit 2026-11-01): this was `**symbol`. `dlsym` can return null without an
/// error (a weak or absolute symbol of that name), and a stray symbol need not be aligned, so a file
/// that is not a game library died of a fault or was undefined behavior here instead of being
/// refused.
///
/// # Safety
///
/// `address` is null, or points to eight readable, initialized bytes. It need not be aligned.
unsafe fn copy_abi_hash(address: *const u64, path: &str) -> Result<u64, GameLibraryError> {
    if address.is_null() {
        return Err(GameLibraryError::MissingSymbol {
            path: path.to_string(),
            symbol: "REGIA_ABI_HASH",
            detail: "symbol address is null".to_string(),
        });
    }
    // SAFETY: the caller's contract; `read_unaligned` makes no alignment assumption.
    Ok(unsafe { address.read_unaligned() })
}

fn read_entry_address(library: &Library, path: &str) -> Result<usize, GameLibraryError> {
    // SAFETY: `get` resolves `regia_game_entry` by name. `try_as_raw_ptr` returns the address
    // the loader produced and does not call the function. The caller leaks the library, so the
    // address stays mapped.
    unsafe {
        let symbol =
            library
                .get::<*mut ()>(GAME_ENTRY_SYMBOL)
                .map_err(|err| GameLibraryError::MissingSymbol {
                    path: path.to_string(),
                    symbol: "regia_game_entry",
                    detail: err.to_string(),
                })?;
        let addr = symbol.try_as_raw_ptr().unwrap_or(std::ptr::null_mut());
        if addr.is_null() {
            return Err(GameLibraryError::MissingSymbol {
                path: path.to_string(),
                symbol: "regia_game_entry",
                detail: "symbol address is null".to_string(),
            });
        }
        Ok(addr.addr())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn game_library_is_opened_with_eager_binding() {
        use libloading::os::unix::{RTLD_GLOBAL, RTLD_LAZY, RTLD_NOW};
        assert_eq!(OPEN_FLAGS & RTLD_NOW, RTLD_NOW);
        assert_eq!(OPEN_FLAGS & RTLD_LAZY, 0);
        // `RTLD_LOCAL` is 0 on glibc, so "local" is checked as "not global".
        assert_eq!(OPEN_FLAGS & RTLD_GLOBAL, 0);
    }

    /// Audit G editor2-platform M1 (sprint-fix-editor2-platform): the matcher that every OS
    /// probe shares must accept the toolchain's file names and nothing else — in particular
    /// not the Windows import library, which can sit next to a loaded dll.
    #[test]
    fn dynamic_std_matcher_accepts_only_loaded_std_libraries() {
        assert!(is_dynamic_std("std-c09cc5becd32eb9a.dll"));
        assert!(is_dynamic_std("libstd-c09cc5becd32eb9a.so"));
        assert!(is_dynamic_std("libstd-c09cc5becd32eb9a.dylib"));
        assert!(!is_dynamic_std("std-c09cc5becd32eb9a.dll.lib"), "import library");
        assert!(
            !is_dynamic_std("libstd-c09cc5becd32eb9a.so.2"),
            "versioned soname"
        );
        assert!(!is_dynamic_std("libstd_utils.so"), "substring in another library");
        assert!(!is_dynamic_std("mystd-c09.dll"), "substring in another prefix");
        assert!(!is_dynamic_std("std-.txt"));
        assert!(!is_dynamic_std("libstd-"), "no extension");
    }

    /// The Linux maps probe reads paths, not substrings: anonymous mappings and `libstd-` in a
    /// directory name must not count; a mapped toolchain `libstd-*.so` must.
    #[test]
    fn maps_probe_reads_the_mapped_file_name() {
        let std_line = "7f2a8c000000-7f2a8c021000 r-xp 00000000 00:190 1234567  /toolchain/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-c09cc5becd32eb9a.so";
        assert!(maps_report_dynamic_std(std_line));
        let mut maps = String::new();
        maps.push_str("7f2a8c000000-7f2a8c021000 rwxp 00000000 00:00 0\n");
        maps.push_str(
            "7f2a8c000000-7f2a8c021000 r-xp 00000000 00:190 1234567  /opt/libstd-bundle/bin/tool.so\n",
        );
        maps.push_str("7f2a8c000000-7f2a8c021000 r-xp 00000000 00:190 1234567  /t/libstd-c09.dll.lib\n");
        maps.push_str("7f2a8c000000-7f2a8c021000 r-xp 00000000 00:190 1234567  /usr/lib/libc.so.6\n");
        assert!(
            !maps_report_dynamic_std(&maps),
            "anonymous mappings, directory substrings and non-std files must not count: {maps}"
        );
        // A mapped-but-unlinked std (the Rust toolchain replaced under a running
        // prefer-dynamic editor) still counts: the ` (deleted)` suffix must not hide the
        // path (audit G editor2-platform L5, sprint-fixrev-08-k).
        let deleted_std = "7f2a8c000000-7f2a8c021000 r-xp 00000000 00:190 1234567  /toolchain/lib/rustlib/x86_64-unknown-linux-gnu/lib/libstd-c09cc5becd32eb9a.so (deleted)";
        assert!(
            maps_report_dynamic_std(deleted_std),
            "a deleted-but-mapped libstd must still count: {deleted_std}"
        );
    }

    /// Audit G editor2-platform M1: `process_links_dynamic_std` must agree with a fresh,
    /// independent read of `/proc/self/maps`. The independent side is a STRUCTURAL parse —
    /// exactly six whitespace-separated fields with an absolute path — matched with its own
    /// `libstd-*.so` predicate, not the implementation's `is_dynamic_std` walk. A statically
    /// linked test binary expects `false` on both sides; run the tests with
    /// `-C prefer-dynamic` and the toolchain lib dir on `LD_LIBRARY_PATH` and both sides flip
    /// to `true`. An implementation that hardcoded either answer fails one of those two runs.
    #[cfg(target_os = "linux")]
    #[test]
    fn process_links_dynamic_std_matches_an_independent_maps_probe() {
        let maps = std::fs::read_to_string("/proc/self/maps").expect("read /proc/self/maps");
        let independent = maps.lines().any(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() != 6 || !fields[5].starts_with('/') {
                return false;
            }
            let name = fields[5].rsplit('/').next().unwrap_or(fields[5]);
            name.starts_with("libstd-") && name.ends_with(".so")
        });
        assert_eq!(
            process_links_dynamic_std(),
            independent,
            "the probe and the independent maps scan must agree"
        );
    }

    /// Windows half of the M1 regression test: the implementation enumerates modules through
    /// the kernel32 `K32*` exports; this probe repeats the enumeration through the psapi entry
    /// points and asserts agreement, so a broken probe fails on Windows CI in either linkage
    /// mode.
    #[cfg(target_os = "windows")]
    #[test]
    fn process_links_dynamic_std_matches_an_independent_module_probe() {
        use windows_sys::Win32::Foundation::HMODULE;
        use windows_sys::Win32::System::ProcessStatus::{EnumProcessModules, GetModuleBaseNameW};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        // SAFETY: `GetCurrentProcess` returns the current-process pseudo handle: it needs no
        // cleanup and is valid for both psapi calls below.
        let process = unsafe { GetCurrentProcess() };
        // Grow to `bytes_needed` like the implementation (audit G editor2-platform L4), so
        // the probes cannot silently agree on a truncated prefix past 1024 modules.
        let module_size = std::mem::size_of::<HMODULE>();
        let mut modules: Vec<HMODULE> = vec![std::ptr::null_mut(); 1024];
        let mut bytes_needed: u32 = 0;
        loop {
            // SAFETY: `modules` is writable for `modules.len()` handles for the call's
            // duration and `bytes_needed` is a valid `u32` out-param.
            let listed = unsafe {
                EnumProcessModules(
                    process,
                    modules.as_mut_ptr(),
                    (modules.len() * module_size) as u32,
                    &mut bytes_needed,
                )
            };
            assert!(listed != 0, "EnumProcessModules failed");
            let needed = usize::try_from(bytes_needed).unwrap_or(0);
            if needed <= modules.len() * module_size {
                break;
            }
            modules.resize(needed / module_size + 1, std::ptr::null_mut());
        }
        let count = usize::try_from(bytes_needed).unwrap_or(0) / module_size;
        let mut wide = [0u16; 512];
        let mut independent = false;
        for module in modules.iter().take(count.min(modules.len())) {
            // SAFETY: `wide` is writable for 512 UTF-16 code units for the call's duration.
            let len = unsafe { GetModuleBaseNameW(process, *module, wide.as_mut_ptr(), wide.len() as u32) };
            if len == 0 {
                continue;
            }
            let end = usize::try_from(len).unwrap_or(0).min(wide.len());
            if is_dynamic_std(&String::from_utf16_lossy(&wide[..end])) {
                independent = true;
                break;
            }
        }
        assert_eq!(
            process_links_dynamic_std(),
            independent,
            "the probe and the independent module enumeration must agree"
        );
    }

    /// Audit G editor2-platform M2 (sprint-fixrev-08-k), macOS half: no runtime test existed
    /// for the dyld probe. The independent side reads dyld's image list through the public
    /// `mach-o/dyld.h` entry points and matches with its own `libstd-*.dylib` predicate, not
    /// the implementation's `is_dynamic_std` walk. A statically linked test binary expects
    /// `false` on both sides; under `-C prefer-dynamic` with `DYLD_LIBRARY_PATH` pointing at
    /// the toolchain lib dir both sides flip to `true`.
    #[cfg(target_os = "macos")]
    #[test]
    fn process_links_dynamic_std_matches_an_independent_dyld_probe() {
        // SAFETY: (the block) both functions are dyld's public `mach-o/dyld.h` API — the
        // header declares them with `uint32_t` parameters — linked into every process by
        // dyld; declaring them introduces no user code.
        unsafe extern "C" {
            fn _dyld_image_count() -> u32;
            fn _dyld_get_image_name(index: u32) -> *const std::ffi::c_char;
        }
        // SAFETY: dyld never unloads images from a running process and Regia never
        // `dlclose`s its dylibs (specs/20 § 3), so a returned name pointer cannot dangle.
        let count = unsafe { _dyld_image_count() };
        let mut independent = false;
        for index in 0..count {
            // SAFETY: `index` is below the live image count the API just reported.
            let name = unsafe { _dyld_get_image_name(index) };
            if name.is_null() {
                continue;
            }
            // SAFETY: `name` is the nul-terminated path dyld returned.
            let bytes = unsafe { std::ffi::CStr::from_ptr(name) }.to_bytes();
            let file_name = bytes.rsplit(|&byte| byte == b'/').next().unwrap_or(bytes);
            if file_name.starts_with(b"libstd-") && file_name.ends_with(b".dylib") {
                independent = true;
                break;
            }
        }
        assert_eq!(
            process_links_dynamic_std(),
            independent,
            "the probe and the independent dyld image list must agree"
        );
    }

    #[test]
    fn missing_file_is_an_error() {
        let path = std::env::temp_dir().join("regia-hot-reload-missing-game-library.so");
        let err = load_game_library(&path, 0).expect_err("a missing file must fail");
        match &err {
            GameLibraryError::MissingFile { path: got } => {
                assert!(got.contains("regia-hot-reload-missing-game-library"), "{got}");
            }
            // A statically linked test process refuses before it looks at the path.
            GameLibraryError::StaticStd => {}
            other => panic!("expected MissingFile, got {other:?}"),
        }
    }

    #[test]
    fn mismatched_abi_hash_is_rejected() {
        let err = reject_abi_mismatch("game.so", 0x111, 0x222).expect_err("mismatch must fail");
        assert_eq!(
            err,
            GameLibraryError::AbiMismatch {
                path: "game.so".to_string(),
                found: 0x111,
                expected: 0x222,
            }
        );
        let message = err.to_string();
        assert!(message.contains("rebuild the editor"), "{message}");
        assert!(message.contains("0000000000000111"), "{message}");
        assert!(message.contains("0000000000000222"), "{message}");
    }

    #[test]
    fn matching_abi_hash_is_accepted() {
        assert!(reject_abi_mismatch("game.so", 7, 7).is_ok());
    }

    #[test]
    fn abi_hash_parser_rejects_junk_and_reads_decimals() {
        assert_eq!(parse_abi_hash("42"), 42);
        assert_eq!(parse_abi_hash(""), 0);
        assert_eq!(parse_abi_hash("12x"), 0);
        assert_eq!(parse_abi_hash(&u64::MAX.to_string()), u64::MAX);
    }

    #[test]
    fn export_table_matches_the_fixture_layout() {
        assert_eq!(size_of::<GameExports>(), size_of::<usize>() * 3);
        assert_eq!(align_of::<GameExports>(), align_of::<usize>());
    }

    static TABLE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn table_register(_: &mut regia_app::prelude::App) {}

    fn table_on_unload(_: &mut regia_app::prelude::World) {
        TABLE_CALLS.fetch_add(10, std::sync::atomic::Ordering::Relaxed);
    }

    fn table_on_load(_: &mut regia_app::prelude::World) {
        TABLE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// `[pad zero bytes][register][on_unload][on_load]` as the bytes of a table, with `null`
    /// replacing slot `null_slot`. The slots are written as pointers, not as integers, so the bytes
    /// keep what a real table's relocations give them and the test also runs under Miri.
    fn table_bytes(pad: usize, null_slot: Option<usize>) -> Vec<u8> {
        let filled = |slot: usize| null_slot != Some(slot);
        let table = RawGameExports {
            register: filled(0).then_some(table_register),
            on_unload: filled(1).then_some(table_on_unload),
            on_load: filled(2).then_some(table_on_load),
        };
        let mut bytes = vec![0u8; pad + size_of::<RawGameExports>()];
        // SAFETY: `bytes[pad..]` is exactly the size of the table; the write needs no alignment.
        unsafe {
            bytes[pad..]
                .as_mut_ptr()
                .cast::<RawGameExports>()
                .write_unaligned(table)
        };
        bytes
    }

    /// Unaligned input (unsafe audit 2026-10-21): a table at an odd address copies out intact
    /// (it was a `ptr::read` that required alignment), and the copied pointers are callable.
    #[test]
    fn an_unaligned_export_table_copies_intact() {
        for pad in [0, 1, 3, size_of::<usize>() - 1] {
            let bytes = table_bytes(pad, None);
            // SAFETY: `bytes[pad..]` holds the three pointer-sized words `copy_export_table` reads.
            let exports = unsafe { copy_export_table(bytes[pad..].as_ptr(), "game.so") }.expect("copy");
            let before = TABLE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
            let mut world = regia_app::prelude::World::new();
            (exports.on_load)(&mut world);
            (exports.on_unload)(&mut world);
            let after = TABLE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
            assert_eq!(after - before, 11, "pad {pad}: on_load then on_unload ran");
        }
    }

    /// Hostile input: a table with any null slot is `NullEntry`, never an invalid function
    /// pointer that a later reload would call.
    #[test]
    fn a_null_slot_in_the_export_table_is_an_error() {
        for null_slot in 0..3 {
            let bytes = table_bytes(1, Some(null_slot));
            // SAFETY: as above, three readable words after one pad byte.
            let copied = unsafe { copy_export_table(bytes[1..].as_ptr(), "game.so") };
            assert!(
                matches!(&copied, Err(GameLibraryError::NullEntry { path }) if path == "game.so"),
                "slot {null_slot} null must be refused"
            );
        }
        let message = GameLibraryError::NullEntry {
            path: "game.so".to_string(),
        }
        .to_string();
        assert!(message.contains("game.so"), "{message}");
    }

    /// A null address (a weak or absolute symbol of that name in a stray file) is an error, and an
    /// odd address reads correctly: both were `**symbol`.
    #[test]
    fn the_abi_hash_copy_refuses_null_and_does_not_need_alignment() {
        // SAFETY: null is allowed by the contract.
        let err = unsafe { copy_abi_hash(std::ptr::null(), "game.so") }.expect_err("null");
        assert!(
            matches!(
                &err,
                GameLibraryError::MissingSymbol {
                    symbol: "REGIA_ABI_HASH",
                    ..
                }
            ),
            "{err:?}"
        );
        let mut bytes = vec![0u8; 8 + size_of::<u64>()];
        for pad in 0..8 {
            bytes[pad..pad + 8].copy_from_slice(&0x0123_4567_89AB_CDEF_u64.to_ne_bytes());
            // SAFETY: eight readable bytes at `pad`.
            let read = unsafe { copy_abi_hash(bytes[pad..].as_ptr().cast(), "game.so") };
            assert_eq!(read, Ok(0x0123_4567_89AB_CDEF), "pad {pad}");
        }
    }

    /// Compiles `source` into a C-ABI library in `dir` with the toolchain the tests were built by;
    /// `None` when this machine has no `rustc` to run (the host's own tests still ran).
    #[cfg(not(target_os = "android"))]
    fn compile_library(dir: &Path, name: &str, source: &str) -> Option<std::path::PathBuf> {
        let source_path = dir.join(format!("{name}.rs"));
        std::fs::write(&source_path, source).expect("write the library source");
        let library = dir.join(format!(
            "{}{name}.{}",
            std::env::consts::DLL_PREFIX,
            dylib_extension()
        ));
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let output = std::process::Command::new(rustc)
            .args([
                "--edition",
                "2024",
                "--crate-type",
                "cdylib",
                "-C",
                "panic=abort",
                "-o",
            ])
            .arg(&library)
            .arg(&source_path)
            .output();
        match output {
            Ok(output) => assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(err) => {
                eprintln!("skipped: no rustc to build the test library ({err})");
                return None;
            }
        }
        Some(library)
    }

    /// A real library through the real loader: the double indirection of `Symbol<*const u64>`, the
    /// entry address, and the export table. The fixture dylib test above needs a `prefer-dynamic`
    /// process; these steps do not.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn a_real_library_goes_through_every_read() {
        let dir = regia_test_utils::unique_temp_dir("hot-reload-real-library");
        let source = r#"
            #[unsafe(no_mangle)]
            #[used]
            pub static REGIA_ABI_HASH: u64 = 0xFEED_0000_0000_0042;

            extern "Rust" fn slot(_: *mut u8) {}

            #[repr(C)]
            pub struct Table([Option<extern "Rust" fn(*mut u8)>; 3]);

            static TABLE: Table = Table([Some(slot), SECOND, Some(slot)]);

            #[unsafe(no_mangle)]
            pub extern "Rust" fn regia_game_entry() -> *const Table {
                &TABLE
            }
        "#;
        let holed = format!("const SECOND: Option<extern \"Rust\" fn(*mut u8)> = None;{source}");
        let Some(path) = compile_library(&dir, "holed", &holed) else {
            return;
        };
        let library = open_library(&path).expect("the library opens");
        let shown = path.display().to_string();
        assert_eq!(read_abi_hash(&library, &shown), Ok(0xFEED_0000_0000_0042));
        assert_ne!(read_entry_address(&library, &shown).expect("entry"), 0);
        let loaded = read_game_symbols(&library, &shown, 0xFEED_0000_0000_0042).expect("matching hash");
        assert_eq!(loaded.abi_hash(), 0xFEED_0000_0000_0042);
        assert!(matches!(
            read_game_symbols(&library, &shown, 1),
            Err(GameLibraryError::AbiMismatch {
                found: 0xFEED_0000_0000_0042,
                expected: 1,
                ..
            })
        ));
        assert!(matches!(
            read_exports(&library, &shown, 1),
            Err(GameLibraryError::AbiMismatch { .. })
        ));
        assert!(
            matches!(
                read_exports(&library, &shown, 0xFEED_0000_0000_0042),
                Err(GameLibraryError::NullEntry { .. })
            ),
            "the table's null slot is refused"
        );
        let full = format!("const SECOND: Option<extern \"Rust\" fn(*mut u8)> = Some(slot);{source}");
        let path = compile_library(&dir, "full", &full).expect("rustc ran a moment ago");
        let library = open_library(&path).expect("the library opens");
        assert!(
            read_exports(&library, &path.display().to_string(), 0xFEED_0000_0000_0042).is_ok(),
            "a full table copies out"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A library with neither symbol is refused by name, and a file that is not a library at all
    /// never gets as far as a symbol.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn a_library_without_the_symbols_and_a_junk_file_are_refused() {
        let dir = regia_test_utils::unique_temp_dir("hot-reload-no-symbols");
        let Some(path) = compile_library(&dir, "plain", "pub extern \"C\" fn unrelated() {}") else {
            return;
        };
        let library = open_library(&path).expect("the library opens");
        let shown = path.display().to_string();
        assert!(matches!(
            read_abi_hash(&library, &shown),
            Err(GameLibraryError::MissingSymbol {
                symbol: "REGIA_ABI_HASH",
                ..
            })
        ));
        assert!(matches!(
            read_entry_address(&library, &shown),
            Err(GameLibraryError::MissingSymbol {
                symbol: "regia_game_entry",
                ..
            })
        ));
        let junk = dir.join(format!("junk.{}", dylib_extension()));
        std::fs::write(&junk, b"this is not a shared object").expect("write");
        assert!(matches!(
            open_library(&junk),
            Err(GameLibraryError::LoadFailed { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dylib_extension_matches_this_os() {
        let ext = dylib_extension();
        if cfg!(windows) {
            assert_eq!(ext, "dll");
        } else if cfg!(target_os = "macos") {
            assert_eq!(ext, "dylib");
        } else {
            assert_eq!(ext, "so");
        }
    }

    /// The fixture's `REGIA_ABI_HASH` when it is built without the variable
    /// (`export_game!`'s framework fallback).
    const FIXTURE_ABI_HASH: u64 = regia_framework::abi_hash_from(None);

    /// Loads `samples/hot_fixture` built as a dylib outside `cargo test`.
    ///
    /// Nesting `cargo rustc` inside `cargo test` deadlocks the package lock when both use the
    /// same target directory. Build first, into its own target directory, with
    /// `-C prefer-dynamic`:
    ///
    /// ```text
    /// cargo rustc -p hot_fixture --lib --crate-type dylib --target-dir <unique-dir> -- -C prefer-dynamic
    /// LD_LIBRARY_PATH=$(rustc --print sysroot)/lib/rustlib/<host>/lib \
    ///   REGIA_HOT_FIXTURE=<unique-dir>/debug/libhot_fixture.so \
    ///   cargo rustc -p regia_editor --lib --profile dev -- -C prefer-dynamic
    /// ```
    #[test]
    #[ignore = "requires REGIA_HOT_FIXTURE pointing at a prefer-dynamic hot_fixture dylib"]
    fn loads_fixture_dylib() {
        let path = regia_core::brand::env_var("REGIA_HOT_FIXTURE").expect(
            "set REGIA_HOT_FIXTURE to the hot_fixture dylib (cargo rustc -p hot_fixture --lib --crate-type dylib)",
        );
        let path = Path::new(&path);
        let loaded = load_game_library(path, FIXTURE_ABI_HASH)
            .expect("fixture dylib should load when the expected hash matches");
        assert_eq!(loaded.abi_hash(), FIXTURE_ABI_HASH);
        assert_ne!(loaded.entry_address(), 0, "regia_game_entry was not resolved");

        let err = load_game_library(path, FIXTURE_ABI_HASH ^ 1).expect_err("a wrong hash must fail");
        match err {
            GameLibraryError::AbiMismatch { found, expected, .. } => {
                assert_eq!(found, FIXTURE_ABI_HASH);
                assert_eq!(expected, FIXTURE_ABI_HASH ^ 1);
            }
            other => panic!("expected AbiMismatch, got {other:?}"),
        }
    }
}
