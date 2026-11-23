// SPDX-License-Identifier: MPL-2.0
//! The ABI stamp of a dynamic extension (`specs/57` § 2.8, ADR-1885).
//!
//! Rust has no stable ABI, so a prebuilt extension library is accepted only when it proves it was
//! built by the same compiler, for the same target and profile, against the same engine build. The
//! library exports `regia_extension_stamp`, an `extern "C"` function returning a pointer to an
//! [`ExtensionStamp`]; the host reads the record **before** resolving any other symbol and
//! compares every field with its own. The first differing field names the `abi_mismatch`.
//!
//! The record has a fixed `#[repr(C)]` layout of [`STAMP_SIZE`] bytes without padding, read
//! little-endian (every Regia target is little-endian). [`ExtensionStamp::from_bytes`] never
//! trusts the bytes: a short read or a bad magic is `not_an_extension`.

use std::fmt;
use std::io::Read as _;
use std::path::{Path, PathBuf};

include!(concat!(env!("OUT_DIR"), "/extension_stamp.rs"));

/// The first eight bytes of every stamp.
pub const STAMP_MAGIC: [u8; 8] = *b"REGIAEXT";
/// The stamp layout version this build writes and reads.
pub const STAMP_VERSION: u32 = 1;
/// Bumped whenever a public type layout reachable from the extension surface changes
/// (`specs/57` § 11.1); dynamic extensions rebuild on each epoch.
pub const ABI_EPOCH: u32 = 1;
/// Size of the record in bytes.
pub const STAMP_SIZE: usize = 232;
/// The engine version the stamp records (the framework crate's version).
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Name of the exported stamp **data** symbol: the record itself, which the host reads from the
/// library file without loading it (ADR-2844).
pub const STAMP_DATA_SYMBOL: &str = "REGIA_EXTENSION_STAMP";
/// Name of the exported stamp function (returns the address of [`STAMP_DATA_SYMBOL`]).
pub const STAMP_SYMBOL: &str = "regia_extension_stamp";
/// Name of the exported entry function, resolved only after the stamp matched.
pub const ENTRY_SYMBOL: &str = "regia_extension_entry";

/// The fixed-layout record a dynamic extension exports (spec § 2.8).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ExtensionStamp {
    /// [`STAMP_MAGIC`].
    pub magic: [u8; 8],
    /// [`STAMP_VERSION`].
    pub stamp_version: u32,
    /// Engine semver, NUL padded.
    pub engine_version: [u8; 32],
    /// [`ABI_EPOCH`] of the engine build.
    pub abi_epoch: u32,
    /// BLAKE3 of `rustc -vV`.
    pub rustc_hash: [u8; 32],
    /// Target triple, NUL padded.
    pub target_triple: [u8; 48],
    /// BLAKE3 of the build profile.
    pub profile_hash: [u8; 32],
    /// The `REGIA_ABI_HASH` of the engine build (`specs/20` § 2).
    pub abi_hash: u64,
    /// The engine dylib fingerprint (`specs/20` § 1): the `u64` little-endian in the first eight
    /// bytes, the rest zero.
    pub engine_fingerprint: [u8; 32],
    /// Hash of the manifest bytes the library embeds: four FNV-1a 64 lanes ([`manifest_hash`],
    /// `const` so the stamp can be a `static`; ADR-1885), not BLAKE3.
    pub manifest_hash: [u8; 32],
}

const _: () = assert!(std::mem::size_of::<ExtensionStamp>() == STAMP_SIZE);

/// One field of the record: name, byte offset and length (in comparison order).
pub const STAMP_FIELDS: [(&str, usize, usize); 10] = [
    ("magic", 0, 8),
    ("stamp_version", 8, 4),
    ("engine_version", 12, 32),
    ("abi_epoch", 44, 4),
    ("rustc_hash", 48, 32),
    ("target_triple", 80, 48),
    ("profile_hash", 128, 32),
    ("abi_hash", 160, 8),
    ("engine_fingerprint", 168, 32),
    ("manifest_hash", 200, 32),
];

/// Why a stamp was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StampError {
    /// Missing symbol, short read or bad magic (`not_an_extension`).
    NotAnExtension(String),
    /// A field differs from the host's (`abi_mismatch`); `field` is the first one in
    /// [`STAMP_FIELDS`] order.
    Mismatch {
        /// The first differing field.
        field: &'static str,
    },
}

impl StampError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotAnExtension(_) => "not_an_extension",
            Self::Mismatch { .. } => "abi_mismatch",
        }
    }
}

impl fmt::Display for StampError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnExtension(detail) => write!(f, "not_an_extension: {detail}"),
            Self::Mismatch { field } => write!(
                f,
                "abi_mismatch: `{field}` differs from this engine build; rebuild the extension with `regia plugin build`"
            ),
        }
    }
}

impl std::error::Error for StampError {}

const fn padded<const N: usize>(text: &str) -> [u8; N] {
    let bytes = text.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    // Keep one NUL at the end so the field always terminates.
    while i < bytes.len() && i + 1 < N {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

const fn fingerprint_bytes(fingerprint: u64) -> [u8; 32] {
    let le = fingerprint.to_le_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 8 {
        out[i] = le[i];
        i += 1;
    }
    out
}

/// `const` decimal parser for build-time pins (`str::parse` is not `const`).
#[must_use]
pub const fn parse_u64(text: Option<&str>) -> u64 {
    let Some(text) = text else { return 0 };
    let bytes = text.as_bytes();
    let mut value: u64 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < b'0' || b > b'9' {
            return 0;
        }
        value = value.wrapping_mul(10).wrapping_add((b - b'0') as u64);
        i += 1;
    }
    value
}

impl ExtensionStamp {
    /// The stamp of this engine build for an extension whose manifest hashes to `manifest_hash`,
    /// with the hot-reload ABI hash and engine fingerprint given by the caller (the editor passes
    /// both to `regia plugin build` as `REGIA_ABI_HASH` and `REGIA_ENGINE_FINGERPRINT`).
    #[must_use]
    pub const fn for_build(abi_hash: u64, engine_fingerprint: u64, manifest_hash: [u8; 32]) -> Self {
        Self {
            magic: STAMP_MAGIC,
            stamp_version: STAMP_VERSION,
            engine_version: padded::<32>(ENGINE_VERSION),
            abi_epoch: ABI_EPOCH,
            rustc_hash: BUILD_RUSTC_HASH,
            target_triple: padded::<48>(BUILD_TARGET),
            profile_hash: BUILD_PROFILE_HASH,
            abi_hash,
            engine_fingerprint: fingerprint_bytes(engine_fingerprint),
            manifest_hash,
        }
    }

    /// The record as bytes (little-endian numbers), the same bytes as its in-memory layout.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; STAMP_SIZE] {
        let mut out = [0u8; STAMP_SIZE];
        out[0..8].copy_from_slice(&self.magic);
        out[8..12].copy_from_slice(&self.stamp_version.to_le_bytes());
        out[12..44].copy_from_slice(&self.engine_version);
        out[44..48].copy_from_slice(&self.abi_epoch.to_le_bytes());
        out[48..80].copy_from_slice(&self.rustc_hash);
        out[80..128].copy_from_slice(&self.target_triple);
        out[128..160].copy_from_slice(&self.profile_hash);
        out[160..168].copy_from_slice(&self.abi_hash.to_le_bytes());
        out[168..200].copy_from_slice(&self.engine_fingerprint);
        out[200..232].copy_from_slice(&self.manifest_hash);
        out
    }

    /// Reads the record from the exported [`STAMP_DATA_SYMBOL`] of a library file (ELF, PE or
    /// Mach-O bytes), without loading it. The record is found through the file's own symbol table
    /// and address mapping, never by searching for the magic, so a copy placed elsewhere in the
    /// file (a doctored string, a second record) is not what the host trusts (ADR-2844).
    ///
    /// # Errors
    ///
    /// [`StampError::NotAnExtension`] when the bytes are no supported object, the symbol is not
    /// exported, its bytes are not backed by the file, or the record is short or has a bad magic.
    pub fn from_object_bytes(file: &[u8]) -> Result<Self, StampError> {
        use super::objfile::{ObjError, ObjFile};
        let object = ObjFile::parse(file).map_err(|e| StampError::NotAnExtension(e.to_string()))?;
        let bytes = object.exported_data(STAMP_DATA_SYMBOL, STAMP_SIZE).map_err(|e| match e {
            ObjError::SymbolMissing => StampError::NotAnExtension(format!(
                "no exported `{STAMP_DATA_SYMBOL}` symbol; rebuild the extension with `regia plugin build`"
            )),
            other => StampError::NotAnExtension(other.to_string()),
        })?;
        Self::from_bytes(bytes)
    }

    /// Finds the first record in the bytes of a library file by searching for the magic. **Not a
    /// trust boundary** (ADR-2844): position proves nothing, so the loader uses
    /// [`Self::from_object_bytes`]. Kept for tooling and the fuzz targets. `None` when no
    /// complete record with the magic is present: a truncated file or a corrupt magic, refused
    /// before any of the library's code can run.
    #[must_use]
    pub fn scan_file_bytes(file: &[u8]) -> Option<Self> {
        file.windows(STAMP_MAGIC.len())
            .enumerate()
            .filter(|(_, window)| *window == STAMP_MAGIC)
            .find_map(|(at, _)| Self::from_bytes(&file[at..]).ok())
    }

    /// Every complete record in a library file. A Rust dylib holds the record twice, as the static
    /// and in its crate metadata, and a good library has them equal.
    #[must_use]
    pub fn scan_file_records(file: &[u8]) -> Vec<Self> {
        file.windows(STAMP_MAGIC.len())
            .enumerate()
            .filter(|(_, window)| *window == STAMP_MAGIC)
            .filter_map(|(at, _)| Self::from_bytes(&file[at..]).ok())
            .collect()
    }

    /// Reads a record. Checks the length and the magic only; [`Self::check_against`] compares.
    ///
    /// # Errors
    ///
    /// [`StampError::NotAnExtension`] for fewer than [`STAMP_SIZE`] bytes or a bad magic.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, StampError> {
        let Some(b) = bytes.get(..STAMP_SIZE) else {
            return Err(StampError::NotAnExtension(format!(
                "stamp is {} bytes, {STAMP_SIZE} expected",
                bytes.len()
            )));
        };
        if b[0..8] != STAMP_MAGIC {
            return Err(StampError::NotAnExtension("bad stamp magic".into()));
        }
        let arr = |range: std::ops::Range<usize>| -> Vec<u8> { b[range].to_vec() };
        let mut stamp = Self {
            magic: STAMP_MAGIC,
            stamp_version: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            engine_version: [0; 32],
            abi_epoch: u32::from_le_bytes([b[44], b[45], b[46], b[47]]),
            rustc_hash: [0; 32],
            target_triple: [0; 48],
            profile_hash: [0; 32],
            abi_hash: u64::from_le_bytes([b[160], b[161], b[162], b[163], b[164], b[165], b[166], b[167]]),
            engine_fingerprint: [0; 32],
            manifest_hash: [0; 32],
        };
        stamp.engine_version.copy_from_slice(&arr(12..44));
        stamp.rustc_hash.copy_from_slice(&arr(48..80));
        stamp.target_triple.copy_from_slice(&arr(80..128));
        stamp.profile_hash.copy_from_slice(&arr(128..160));
        stamp.engine_fingerprint.copy_from_slice(&arr(168..200));
        stamp.manifest_hash.copy_from_slice(&arr(200..232));
        Ok(stamp)
    }

    /// Compares every field with `expected` in [`STAMP_FIELDS`] order.
    ///
    /// # Errors
    ///
    /// [`StampError::Mismatch`] naming the first differing field.
    pub fn check_against(&self, expected: &Self) -> Result<(), StampError> {
        let (a, b) = (self.to_bytes(), expected.to_bytes());
        for (field, offset, len) in STAMP_FIELDS {
            if a[offset..offset + len] != b[offset..offset + len] {
                return Err(StampError::Mismatch { field });
            }
        }
        Ok(())
    }

    /// The engine fingerprint as the `u64` it was built from.
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut le = [0u8; 8];
        le.copy_from_slice(&self.engine_fingerprint[..8]);
        u64::from_le_bytes(le)
    }

    /// The engine version text without padding.
    #[must_use]
    pub fn engine_version_str(&self) -> String {
        text_of(&self.engine_version)
    }

    /// The target triple text without padding.
    #[must_use]
    pub fn target_triple_str(&self) -> String {
        text_of(&self.target_triple)
    }

    /// The fields as JSON-friendly strings (hex for hashes), for `plugin.info`.
    #[must_use]
    pub fn describe(&self) -> Vec<(&'static str, String)> {
        vec![
            ("stamp_version", self.stamp_version.to_string()),
            ("engine_version", self.engine_version_str()),
            ("abi_epoch", self.abi_epoch.to_string()),
            ("rustc_hash", hex(&self.rustc_hash)),
            ("target_triple", self.target_triple_str()),
            ("profile_hash", hex(&self.profile_hash)),
            ("abi_hash", self.abi_hash.to_string()),
            ("engine_fingerprint", hex(&self.engine_fingerprint[..8])),
            ("manifest_hash", hex(&self.manifest_hash)),
        ]
    }
}

impl fmt::Debug for ExtensionStamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("ExtensionStamp");
        for (name, value) in self.describe() {
            s.field(name, &value);
        }
        s.finish()
    }
}

fn text_of(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The value of [`ExtensionStamp::manifest_hash`]: four FNV-1a 64 lanes with different seeds.
///
/// `const` so `regia::extension!` can bake the whole stamp into a `static` of the library, where
/// the host reads it without running extension code. It spots a library built from another
/// manifest; it is not a cryptographic hash, and nothing here is a security boundary.
#[must_use]
pub const fn manifest_hash(bytes: &[u8]) -> [u8; 32] {
    const SEEDS: [u64; 4] = [
        0xcbf2_9ce4_8422_2325,
        0x9e37_79b9_7f4a_7c15,
        0xc2b2_ae3d_27d4_eb4f,
        0x1656_67b1_9e37_79f9,
    ];
    let mut out = [0u8; 32];
    let mut lane = 0;
    while lane < 4 {
        let mut hash = SEEDS[lane];
        let mut i = 0;
        while i < bytes.len() {
            hash ^= bytes[i] as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            i += 1;
        }
        let le = hash.to_le_bytes();
        let mut b = 0;
        while b < 8 {
            out[lane * 8 + b] = le[b];
            b += 1;
        }
        lane += 1;
    }
    out
}

/// The `REGIA_ABI_HASH` of an engine build (`specs/20` § 2; `regia_editor/build.rs` bakes the same
/// value), from the first line of `rustc --version`, the target triple and the cargo profile name
/// (`debug` or `release`). `regia plugin build` passes it to the extension build.
#[must_use]
pub fn abi_hash_for(rustc_version: &str, target: &str, profile: &str) -> u64 {
    let text = format!(
        "{}|regia|{ENGINE_VERSION}|{target}|{profile}",
        rustc_version.trim()
    );
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Hash of the engine dylib in `target_dir/<profile>/` and its `deps/` (`specs/20` § 1), the value
/// of [`ExtensionStamp::engine_fingerprint`]. `profile` is `debug` or `release`. Falls back to the
/// engine rlibs when no `regia_dylib` was built, and to 0 when there are neither. The hash reads
/// bytes, not mtimes: a relink that reproduces the same bytes is not an engine change.
#[must_use]
pub fn engine_fingerprint(target_dir: &Path, profile: &str) -> u64 {
    let profile = target_dir.join(profile);
    let mut paths = dir_files(&profile.join("deps"));
    // The final `libregia_dylib.<ext>` sits next to the deps dir. Either file is the engine copy.
    paths.extend(dir_files(&profile));
    let mut files: Vec<PathBuf> = paths
        .iter()
        .filter(|path| is_engine_dylib(path))
        .cloned()
        .collect();
    if files.is_empty() {
        files = paths.into_iter().filter(|path| is_engine_rlib(path)).collect();
    }
    files.sort();
    if files.is_empty() {
        return 0;
    }
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for path in files {
        hash = mix(
            hash,
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("")
                .as_bytes(),
        );
        hash = mix_file(hash, &path);
    }
    hash
}

fn dir_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect()
}

/// Hashes the length and bytes of `path` into `hash`. An unreadable file mixes nothing.
fn mix_file(mut hash: u64, path: &Path) -> u64 {
    let Ok(mut file) = std::fs::File::open(path) else {
        return hash;
    };
    if let Ok(meta) = file.metadata() {
        hash = mix(hash, &meta.len().to_le_bytes());
    }
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => hash = mix(hash, &buffer[..read]),
        }
    }
    hash
}

/// The dynamic-library file extension of this OS (`so`, `dll`, `dylib`).
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

fn is_engine_dylib(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let ext = format!(".{}", dylib_extension());
    let Some(stem) = name.strip_suffix(&ext) else {
        return false;
    };
    // Linux/macOS: libregia_dylib-<hash>.so. Windows: regia_dylib-<hash>.dll.
    stem == "libregia_dylib"
        || stem == "regia_dylib"
        || stem.starts_with("libregia_dylib-")
        || stem.starts_with("regia_dylib-")
}

fn is_engine_rlib(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.ends_with(".rlib")
        && (name.starts_with("libregia-")
            || name.starts_with("libregia_app-")
            || name.starts_with("libregia_ecs-")
            || name.starts_with("libregia_dylib"))
}

fn mix(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_offsets_match_the_field_table() {
        let mut s = ExtensionStamp::for_build(7, 9, [3; 32]);
        s.stamp_version = 0x0102_0304;
        let bytes = s.to_bytes();
        assert_eq!(&bytes[0..8], b"REGIAEXT");
        assert_eq!(&bytes[8..12], &[4, 3, 2, 1]);
        assert_eq!(&bytes[160..168], &7u64.to_le_bytes());
        assert_eq!(bytes[168], 9);
        assert_eq!(&bytes[200..232], &[3; 32]);
        let mut end = 0;
        for (_, offset, len) in STAMP_FIELDS {
            assert_eq!(offset, end, "fields are contiguous");
            end = offset + len;
        }
        assert_eq!(end, STAMP_SIZE);
        assert_eq!(std::mem::offset_of!(ExtensionStamp, abi_hash), 160);
        assert_eq!(std::mem::offset_of!(ExtensionStamp, manifest_hash), 200);
    }

    #[test]
    fn round_trip_and_first_difference() {
        let host = ExtensionStamp::for_build(11, 22, manifest_hash(b"id = \"x\""));
        let back = ExtensionStamp::from_bytes(&host.to_bytes()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, host);
        assert_eq!(back.check_against(&host), Ok(()));
        for (field, offset, _) in STAMP_FIELDS.iter().skip(1) {
            let mut bytes = host.to_bytes();
            bytes[*offset] ^= 0x5a;
            let other = ExtensionStamp::from_bytes(&bytes).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(
                other.check_against(&host),
                Err(StampError::Mismatch { field }),
                "{field}"
            );
        }
        let mut bad = host.to_bytes();
        bad[0] ^= 1;
        assert_eq!(
            ExtensionStamp::from_bytes(&bad).map_err(|e| e.code()),
            Err("not_an_extension")
        );
        assert_eq!(
            ExtensionStamp::from_bytes(&host.to_bytes()[..100]).map_err(|e| e.code()),
            Err("not_an_extension")
        );
    }

    #[test]
    fn the_record_is_found_in_file_bytes() {
        let host = ExtensionStamp::for_build(1, 2, [3; 32]);
        let mut file = vec![0u8; 64];
        file.extend_from_slice(b"REGIA");
        file.extend_from_slice(&host.to_bytes());
        file.extend_from_slice(&[9; 16]);
        assert_eq!(ExtensionStamp::scan_file_bytes(&file), Some(host));
        assert_eq!(
            ExtensionStamp::scan_file_bytes(&file[..200]),
            None,
            "cut inside the record"
        );
        file[69] ^= 1;
        assert_eq!(ExtensionStamp::scan_file_bytes(&file), None, "bad magic");
    }

    #[test]
    fn manifest_hash_runs_at_compile_time() {
        const HASH: [u8; 32] = manifest_hash(b"id = \"x\"");
        assert_eq!(HASH, manifest_hash(b"id = \"x\""));
        assert_ne!(HASH, manifest_hash(b"id = \"y\""));
        assert_ne!(HASH[..8], HASH[8..16], "the lanes differ");
    }

    #[test]
    fn abi_hash_matches_the_hot_reload_recipe() {
        // FNV-1a 64 of the text `regia_editor/build.rs` hashes.
        let hash = abi_hash_for(
            "rustc 1.99.0 (abc 2026-01-01)\n",
            "x86_64-unknown-linux-gnu",
            "debug",
        );
        let text =
            format!("rustc 1.99.0 (abc 2026-01-01)|regia|{ENGINE_VERSION}|x86_64-unknown-linux-gnu|debug");
        assert_eq!(hash, mix(0xcbf2_9ce4_8422_2325, text.as_bytes()));
    }

    #[test]
    fn fingerprint_prefers_the_dylib_and_reads_bytes() {
        let dir = regia_test_utils::unique_temp_dir("ext-fingerprint");
        let deps = dir.join("debug/deps");
        std::fs::create_dir_all(&deps).expect("deps");
        assert_eq!(engine_fingerprint(&dir, "debug"), 0, "no engine artifact");
        std::fs::write(deps.join("libregia_app-1.rlib"), b"rlib").expect("write");
        let rlib_only = engine_fingerprint(&dir, "debug");
        assert_ne!(rlib_only, 0);
        let dylib = deps.join(format!("libregia_dylib-1.{}", dylib_extension()));
        std::fs::write(&dylib, b"one").expect("write");
        let first = engine_fingerprint(&dir, "debug");
        assert_ne!(first, rlib_only);
        std::fs::write(&dylib, b"two").expect("write");
        assert_ne!(first, engine_fingerprint(&dir, "debug"));
        std::fs::write(&dylib, b"one").expect("write");
        assert_eq!(first, engine_fingerprint(&dir, "debug"), "same bytes, same print");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn padding_keeps_a_terminator() {
        let long = "x".repeat(80);
        let p = padded::<48>(&long);
        assert_eq!(p[47], 0);
        assert_eq!(parse_u64(Some("123")), 123);
        assert_eq!(parse_u64(Some("12a")), 0);
        assert_eq!(parse_u64(None), 0);
    }

    #[test]
    fn the_record_is_read_from_the_exported_symbol_in_each_format() {
        use super::super::objfile::synth;
        let stamp = ExtensionStamp::for_build(7, 9, [3; 32]);
        let record = stamp.to_bytes();
        for bytes in [
            synth::elf64(&[(STAMP_DATA_SYMBOL, &record)], &[]),
            synth::pe64(&[(STAMP_DATA_SYMBOL, &record)], &[]),
            synth::macho64(&[(STAMP_DATA_SYMBOL, &record)], &[]),
        ] {
            assert_eq!(ExtensionStamp::from_object_bytes(&bytes), Ok(stamp));
        }
    }

    #[test]
    fn a_record_that_is_not_the_exported_symbol_is_not_a_stamp() {
        use super::super::objfile::synth;
        let record = ExtensionStamp::for_build(7, 9, [3; 32]).to_bytes();
        // Right bytes under another name, the right name with a bad magic, a short symbol, a raw record.
        let mut bad_magic = record;
        bad_magic[0] ^= 1;
        for bytes in [
            synth::elf64(&[("other_name", &record)], &[]),
            synth::elf64(&[(STAMP_DATA_SYMBOL, &bad_magic)], &[]),
            synth::elf64(&[(STAMP_DATA_SYMBOL, &record[..100])], &[]),
            synth::pe64(&[(STAMP_DATA_SYMBOL, &bad_magic)], &[]),
            record.to_vec(),
            Vec::new(),
        ] {
            let error = ExtensionStamp::from_object_bytes(&bytes).expect_err("refused");
            assert_eq!(error.code(), "not_an_extension", "{error}");
        }
    }
}
