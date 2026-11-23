//! Host side of specs/20 § 3 steps 2 and 6: copy the built library to a unique name, load it, check
//! the ABI hash, call the entry. Libraries are leaked (never unloaded, step 9).

use std::path::{Path, PathBuf};

use crate::hot::{ABI_HASH, ABI_SYMBOL, ENTRY_SYMBOL, GameEntryFn, GamePlugin};

pub use libloading;
pub use serde_json;

/// Why a game library could not be loaded.
#[derive(Debug)]
pub enum LoadError {
    /// Copy or open failed.
    Io(String),
    /// Symbol missing.
    Symbol(String),
    /// Built against a different engine.
    AbiMismatch {
        /// The library's hash.
        library: u64,
        /// The host's hash.
        host: u64,
    },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot load game library: {e}"),
            Self::Symbol(e) => write!(f, "game library has no entry point: {e}"),
            Self::AbiMismatch { library, host } => write!(
                f,
                "game library was built against another engine build ({library:016x} != {host:016x}); rebuild the editor"
            ),
        }
    }
}

/// Copies `built` to `hot_dir/<stem>-<counter>.<ext>` (the loader caches by path).
pub fn copy_unique(built: &Path, hot_dir: &Path, counter: u32) -> Result<PathBuf, LoadError> {
    std::fs::create_dir_all(hot_dir).map_err(|e| LoadError::Io(e.to_string()))?;
    let stem = built.file_stem().and_then(|s| s.to_str()).unwrap_or("game");
    let ext = built.extension().and_then(|s| s.to_str()).unwrap_or("");
    let dest = hot_dir.join(format!("{stem}-{counter}.{ext}"));
    std::fs::copy(built, &dest).map_err(|e| LoadError::Io(format!("{}: {e}", dest.display())))?;
    Ok(dest)
}

/// Loads a game library copy, checks `REGIA_ABI_HASH`, calls the entry. The library is leaked.
pub fn load_game(path: &Path) -> Result<Box<dyn GamePlugin>, LoadError> {
    // SAFETY: loading a Rust dylib runs its initialisers. The library was built by the same cargo
    // invocation family against the engine dylib this process already has loaded; the ABI hash
    // below rejects libraries built against another engine build.
    #[allow(unsafe_code)]
    let lib = unsafe { libloading::Library::new(path) }.map_err(|e| LoadError::Io(e.to_string()))?;
    // SAFETY: `export_game!` defines `REGIA_ABI_HASH` as a `u64` static.
    #[allow(unsafe_code)]
    let hash = unsafe {
        let sym = lib
            .get::<*const u64>(ABI_SYMBOL)
            .map_err(|e| LoadError::Symbol(e.to_string()))?;
        **sym
    };
    if hash != ABI_HASH {
        return Err(LoadError::AbiMismatch {
            library: hash,
            host: ABI_HASH,
        });
    }
    // SAFETY: `export_game!` defines the entry with exactly this signature.
    #[allow(unsafe_code)]
    let plugin = unsafe {
        let entry = lib
            .get::<GameEntryFn>(ENTRY_SYMBOL)
            .map_err(|e| LoadError::Symbol(e.to_string()))?;
        entry()
    };
    // Never unload (specs/20 § 3 step 9).
    std::mem::forget(lib);
    Ok(plugin)
}

/// Resident set size of this process in bytes (0 if unknown).
pub fn rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map_or(0, |pages| pages * 4096)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c: PROCESS_MEMORY_COUNTERS = PROCESS_MEMORY_COUNTERS {
            cb: 0,
            PageFaultCount: 0,
            PeakWorkingSetSize: 0,
            WorkingSetSize: 0,
            QuotaPeakPagedPoolUsage: 0,
            QuotaPagedPoolUsage: 0,
            QuotaPeakNonPagedPoolUsage: 0,
            QuotaNonPagedPoolUsage: 0,
            PagefileUsage: 0,
            PeakPagefileUsage: 0,
        };
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: valid pointer and size for the current process handle.
        #[allow(unsafe_code)]
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
        if ok != 0 { c.WorkingSetSize as u64 } else { 0 }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        0
    }
}
