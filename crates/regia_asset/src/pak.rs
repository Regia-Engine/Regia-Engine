// SPDX-License-Identifier: MPL-2.0
//! Pak file reader: FPAK archive format for shipping cooked assets
//! (`docs/specs/12-assets.md` § 5).
//!
//! # Format
//!
//! ```text
//! FPAK header: magic b"FPAK", version u32, flags u32, toc_offset u64, toc_len u64,
//!              content_hash [u8; 32]
//! data: per-asset blobs, each aligned to 4096, optionally compressed with lz4_flex
//!       in 256 KiB blocks
//! TOC (postcard): table-of-contents + asset manifest + pak dependency list
//! ```
//!
//! # Platform backends
//!
//! - Desktop: `memmap2` the whole file (never copied: asset pages fault in on first read),
//!   decompress on demand via the IoPool.
//! - Android: `AAsset` streaming reads through `regia_platform`.
//!
//! # Zero-copy gate
//!
//! A mapped pak serves an entry stored raw as a view of the mapping ([`PakReader::read_shared`]),
//! which lives as long as the asset. A file truncated in place under such a view raises `SIGBUS`
//! on the next access. [`ZeroCopyGate`] closes that window on the first launch after an update:
//! the asset server closes the gate of a pak that has not passed the whole-file check
//! (`pak_verify`) with this build, and while it is closed every read goes through the file handle
//! (`pread`), never the mapping, so a truncated file is an `Err`, not a signal. The check opens
//! the gate when the hash matches. A pak opened by [`PakReader::open`] starts with the gate open.
//!
//! Independently of the gate, every read `fstat`s the open handle (about 1 µs): a file seen shorter
//! or re-stamped since `open` logs one warning and is read through the handle from then on, even
//! with the gate open, so a pak cut in place under a running game is an `Err` on the cut entry,
//! not a `SIGBUS`. Views already served stay exposed.
//!
//! What the gate cannot cover: a view served while the gate was open reads the mapping directly
//! for as long as the asset lives, so a pak truncated or rewritten in place *after* that (by
//! another process: the engine itself only ever replaces a pak by rename) can still fault
//! (`SIGBUS`) or change bytes under a live `&[u8]`. On Windows [`PakReader::open`] denies other
//! writers while the pak is open (and the OS refuses to truncate a mapped file), so only POSIX
//! hosts carry this residue; it is the documented trade-off of zero-copy loading
//! (`docs/specs/12-assets.md` § 5). Android reads paks into owned buffers and never maps.
//!
//! Compressed entries of [`PARALLEL_LZ4_MIN_BLOCKS`] blocks or more decompress their blocks in
//! parallel on the compute pool (`regia_core::tasks`), straight into the output buffer.

use std::borrow::Cow;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use postcard;
use serde::{Deserialize, Serialize};

use crate::id::AssetId;
use crate::manifest::ManifestEntry;
use regia_core::id::TypePathHash;

/// Magic bytes at the start of every `.fpak` file.
pub const FPAK_MAGIC: [u8; 4] = *b"FPAK";

/// Current pak format version.
pub const FPAK_VERSION: u32 = 1;

/// Asset data alignment inside a pak file.
pub const FPAK_BLOB_ALIGNMENT: u64 = 4096;

/// Block size for lz4_flex compression of individual asset blobs.
pub const FPAK_COMPRESS_BLOCK_SIZE: usize = 256 * 1024; // 256 KiB

/// Maximum ratio of an entry's declared (uncompressed) size to its stored compressed bytes.
///
/// Each block is individually capped at [`FPAK_COMPRESS_BLOCK_SIZE`] when it is decompressed, but
/// a chain of tiny, maximally-compressible blocks — a 256 KiB run of zeros stores in ~1043 bytes,
/// ~251:1 — would otherwise inflate to `N × 256 KiB` of memory from a few kilobytes on disk. Real
/// lz4-compressed game data sits below ~10:1, so this bounds the worst-case amplification to a
/// small, fixed multiple and turns a decompression bomb into an `Err`.
pub const FPAK_MAX_BLOCK_RATIO: usize = 64;

/// Largest output one compressed entry may declare. [`FPAK_MAX_BLOCK_RATIO`] bounds the output by the
/// stored bytes, which still lets a 256 MiB file ask for 16 GiB in one allocation. A game with larger
/// entries raises it with [`PakReader::set_max_entry_bytes`]; a reader of mods lowers it.
pub const FPAK_MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// A compressed entry of at least this many blocks (2 MiB of output) decompresses its blocks in
/// parallel on the compute pool; a smaller one on the calling thread.
pub const PARALLEL_LZ4_MIN_BLOCKS: usize = 8;

/// Blocks per parallel decompression task (1 MiB of output).
const LZ4_BLOCKS_PER_TASK: usize = 4;

/// Flags field bits.
pub mod flags {
    /// Assets in this pak use lz4 block compression.
    pub const COMPRESSED: u32 = 1 << 0;
    /// The TOC itself is stored compressed (future-proof; not used in v1).
    pub const TOC_COMPRESSED: u32 = 1 << 1;
}

/// A single entry in the pak table of contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PakEntry {
    /// Asset id.
    pub id: AssetId,
    /// Reflect path hash of the cooked payload type.
    pub type_tag: TypePathHash,
    /// Byte offset from the start of the file to the asset data block.
    pub offset: u64,
    /// Uncompressed byte length of the asset payload.
    pub size: u64,
    /// Compressed byte length on disk (equal to `size` when stored uncompressed).
    pub compressed_size: u64,
    /// When compressed, the byte offset in the file of the block-offset table
    /// (packed u32 LE, one entry per 256 KiB block).  0 when stored uncompressed.
    pub block_table_offset: u64,
}

/// The table of contents stored at the end of a pak file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PakToc {
    /// Every asset stored in this pak.
    pub entries: Vec<PakEntry>,
    /// Path ↔ id manifest so `load_path` works from a pak.
    pub manifest: Vec<ManifestEntry>,
    /// List of pak file names this pak depends on. Mounting orders by it: every listed
    /// dependency mounts before this pak (matched by case-folded file name or stem; unmatched
    /// entries are ignored; the paks of a cycle keep the file-name order among themselves). Runtime
    /// chunk mounts keep the order too. spec 12 § 5.
    #[serde(default)]
    pub dependencies: Vec<String>,
}

/// The serde field-list fingerprint of the `.fpak` format (hashed by `cargo xtask check-formats`).
///
/// The header field list is derived from [`PakHeader`] itself — the one struct
/// [`PakHeader::from_bytes`] reads and [`encode_pak_header`] writes — not a handwritten list or a
/// second mirror struct. Adding a header field to the parser therefore changes this fingerprint
/// without touching a separate type. The table of contents is captured from [`PakToc`],
/// [`PakEntry`] and the manifest entry. `magic` and `version` are deliberately absent: they are
/// fixed identifiers validated against [`FPAK_MAGIC`] / [`FPAK_VERSION`], and the version is
/// tracked separately by `check-formats`.
pub fn pak_schema_fingerprint() -> String {
    regia_core::schema::join_field_lists(&[
        regia_core::schema::serde_field_names::<PakHeader>(),
        regia_core::schema::serde_field_names::<PakToc>(),
        regia_core::schema::serde_field_names::<PakEntry>(),
        regia_core::schema::serde_field_names::<crate::manifest::ManifestEntry>(),
    ])
}

/// Pak files have no built-in migration steps (the format is frozen at v1; an older version is
/// rejected rather than migrated).
pub fn pak_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// Errors that can occur reading a pak file.
#[derive(Debug, thiserror::Error)]
pub enum PakError {
    /// The file does not start with `FPAK` magic.
    #[error("not a regia pak file (bad magic)")]
    BadMagic,

    /// The pak version is not the one this runtime reads (newer needs a newer engine; older needs
    /// a migrator, and none exists yet — the format is frozen at v1).
    #[error("unsupported pak version {found} (supported: {supported})")]
    UnsupportedVersion {
        /// Version in the file.
        found: u32,
        /// Version this runtime reads.
        supported: u32,
    },

    /// The header sets a flag bit this runtime does not read (`TOC_COMPRESSED`, reserved for a
    /// later version, or an unknown bit): the file is not decoded under the wrong assumptions.
    #[error("unsupported pak flags {flags:#x} (this runtime reads: {supported:#x})")]
    UnsupportedFlags {
        /// Flags in the file.
        flags: u32,
        /// Flag bits this runtime reads.
        supported: u32,
    },

    /// The TOC could not be decoded from postcard.
    #[error("pak TOC decode error: {0}")]
    TocDecode(postcard::Error),

    /// Requested asset id is not in this pak.
    #[error("asset {0} not found in pak")]
    NotFound(AssetId),

    /// The content hash does not match (file may be corrupted). `PakReader::open` does not check
    /// the content hash today, so this is not produced by it; use `pak_verify` and `PakIntegrity`
    /// to detect a corrupted pak.
    #[error("pak content hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        /// Expected hash from the header.
        expected: String,
        /// Computed hash.
        actual: String,
    },

    /// IO error.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// An entry's stored bytes are corrupt: a bad block table or lz4 block, a stored length that
    /// disagrees with the entry's layout, or decompressed output of the wrong length.
    #[error("corrupt pak entry {id} in {path} at offset {offset}: {reason}")]
    Corrupt {
        /// Pak file (diagnostics).
        path: String,
        /// Asset id of the entry.
        id: AssetId,
        /// Byte offset of the entry's stored data in the file.
        offset: u64,
        /// What is wrong.
        reason: String,
    },

    /// The header's TOC range does not fit in the file (truncated or still being written).
    #[error("pak TOC [{toc_offset}..+{toc_len}) lies outside the {file_len}-byte file (truncated?)")]
    TocOutOfBounds {
        /// TOC offset from the header.
        toc_offset: u64,
        /// TOC length from the header.
        toc_len: u64,
        /// File length.
        file_len: u64,
    },

    /// Asset offset/size out of bounds.
    #[error("pak entry {id} has out-of-bounds range: offset={offset} size={size} file_len={file_len}")]
    OutOfBounds {
        /// Asset id.
        id: AssetId,
        /// Byte offset in the file.
        offset: u64,
        /// Byte length.
        size: u64,
        /// Total file length.
        file_len: u64,
    },
}

/// Parsed header of a pak file (memory-mapped or read from an Android asset).
///
/// This is the single source of truth for the pak header's *shape*: [`PakHeader::from_bytes`] reads
/// these fields and [`encode_pak_header`] writes them, and [`pak_schema_fingerprint`] derives its
/// field list from this struct, so a header-layout change (adding a field) changes the fingerprint
/// without editing a second type. The header's fixed `magic` and `version` words are validated by
/// [`PakHeader::from_bytes`] but are not shape fields (see [`pak_schema_fingerprint`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PakHeader {
    /// Raw flags from the header.
    pub flags: u32,
    /// Offset of the TOC section from the start of the file.
    pub toc_offset: u64,
    /// Byte length of the TOC section.
    pub toc_len: u64,
    /// blake3 hash of the content (everything before the TOC).
    pub content_hash: [u8; 32],
}

impl PakHeader {
    /// Parse a header from the first 60 bytes of a pak file.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PakError> {
        if bytes.len() < 60 || bytes[..4] != FPAK_MAGIC {
            return Err(PakError::BadMagic);
        }
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        // Only the current version is readable: a newer file needs a newer engine, and an older
        // file needs a migrator (none exist yet — the format is frozen at v1).
        if version != FPAK_VERSION {
            return Err(PakError::UnsupportedVersion {
                found: version,
                supported: FPAK_VERSION,
            });
        }
        let flags = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        // v1 readers know one flag (audit-v035-formats 14): a compressed TOC or an unknown bit
        // is refused here, not misread as a corrupt TOC or served under wrong assumptions.
        if flags & !flags::COMPRESSED != 0 {
            return Err(PakError::UnsupportedFlags {
                flags,
                supported: flags::COMPRESSED,
            });
        }
        let toc_offset = u64::from_le_bytes([
            bytes[12], bytes[13], bytes[14], bytes[15], bytes[16], bytes[17], bytes[18], bytes[19],
        ]);
        let toc_len = u64::from_le_bytes([
            bytes[20], bytes[21], bytes[22], bytes[23], bytes[24], bytes[25], bytes[26], bytes[27],
        ]);
        let content_hash = bytes[28..60].try_into().expect("slice is exactly 32 bytes"); // invariant: fixed-width slice
        Ok(Self {
            flags,
            // Already host order: `from_le_bytes` converted them (a second `from_le` swapped them
            // back on a big-endian host, audit v034 net L7).
            toc_offset,
            toc_len,
            content_hash,
        })
    }

    /// Whether assets are lz4 block-compressed.
    pub fn is_compressed(&self) -> bool {
        self.flags & flags::COMPRESSED != 0
    }

    /// Encode this header (and the fixed magic/version words) into the 60-byte on-disk layout.
    ///
    /// The inverse of [`PakHeader::from_bytes`]. Both read and write the *same* [`PakHeader`]
    /// shape fields, so a field added to the struct forces both paths (and the fingerprint) to
    /// change together.
    pub fn encode(&self, version: u32) -> [u8; 60] {
        let mut out = [0u8; 60];
        out[..4].copy_from_slice(&FPAK_MAGIC);
        out[4..8].copy_from_slice(&version.to_le_bytes());
        out[8..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..20].copy_from_slice(&self.toc_offset.to_le_bytes());
        out[20..28].copy_from_slice(&self.toc_len.to_le_bytes());
        out[28..60].copy_from_slice(&self.content_hash);
        out
    }
}

/// The bytes behind a [`PakReader`]: a read-only file mapping (desktop) or an owned buffer
/// (Android `AAsset` reads, tests).
///
/// A mapped pak is never copied: an asset read touches only that asset's pages, so opening a
/// multi-gigabyte pak costs a TOC parse, not a full read (`docs/specs/12-assets.md` § 5).
enum PakBytes {
    /// `mmap` of the whole file. Paks are shipping files replaced by rename (a new inode), so a
    /// live mapping keeps the old file. A pak truncated in place is not prevented: see the
    /// module docs (zero-copy gate) and ADR-177 / ADR-285 for what is caught and what is not.
    Mapped(memmap2::Mmap),
    /// Bytes the caller already holds.
    Owned(Vec<u8>),
}

impl std::ops::Deref for PakBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Mapped(map) => map,
            Self::Owned(bytes) => bytes,
        }
    }
}

/// Whether a [`PakReader`] may serve payloads as views of its file mapping (see the module docs).
///
/// A cheap clonable handle: the asset server hands a clone to the background integrity check,
/// which opens it once the pak's hash matches.
#[derive(Clone, Debug)]
pub struct ZeroCopyGate(Arc<AtomicBool>);

impl ZeroCopyGate {
    /// A gate in the given state.
    #[must_use]
    pub fn new(open: bool) -> Self {
        Self(Arc::new(AtomicBool::new(open)))
    }

    /// Whether views of the mapping may be served.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Allows views of the mapping (the pak passed its whole-file check).
    pub fn open(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Serves copies read through the file handle from now on (already served views stay valid).
    pub fn close(&self) {
        self.0.store(false, Ordering::Release);
    }
}

/// An open pak file on desktop (memory-mapped) or Android (AAsset reference).
///
/// On desktop this holds an `mmap` of the file; on Android it wraps an
/// `AAsset` that `regia_platform` opens.
pub struct PakReader {
    /// The raw file bytes (mapped on desktop, read into memory from AAsset on Android).
    data: PakBytes,
    /// The file behind a mapped pak: reads go through it while [`Self::zero_copy`] is closed.
    file: Option<std::fs::File>,
    /// Whether payloads may be views of the mapping.
    zero_copy: ZeroCopyGate,
    /// The file's length and modification time when it was opened (`None` for owned bytes).
    opened: Option<(u64, Option<std::time::SystemTime>)>,
    /// Set (and never cleared) once the file is seen shorter or re-stamped: from then on every
    /// read goes through the file handle, whatever the gate says. Separate from the gate so the
    /// verify thread cannot reopen it.
    changed: std::sync::atomic::AtomicBool,
    /// Entry index by asset id (the TOC is a list; lookups are per asset read).
    index: std::collections::HashMap<AssetId, usize>,
    /// Largest declared output of one compressed entry ([`FPAK_MAX_ENTRY_BYTES`] by default).
    max_entry_bytes: u64,
    /// Parsed table of contents.
    pub toc: PakToc,
    /// Header fields.
    pub header: PakHeader,
    /// File path for diagnostics.
    pub path: String,
}

impl PakReader {
    /// Open a `.fpak` file from a path (desktop: `memmap2`, no copy).
    ///
    /// Only the header and the TOC are read here; asset bytes are paged in on first access.
    ///
    /// # Note
    ///
    /// On Android, prefer [`PakReader::from_bytes`] which is fed by
    /// `regia_platform` AAsset reads.
    pub fn open(path: &Path) -> Result<Self, PakError> {
        let _span = tracing::info_span!("pak_open").entered();
        let opened = open_for_mapping(path);
        crate::access_trace::note_io(path, &opened);
        let file = opened?;
        let file_len = file.metadata()?.len();
        if file_len < 60 {
            return Err(PakError::BadMagic);
        }

        // SAFETY: `memmap2::Mmap::map` is sound as long as nothing modifies or truncates the file
        // while the mapping (or a `&[u8]` borrowed from it) is alive. The mapping is read-only and
        // lives exactly as long as this reader; every slice taken from it is bounds-checked
        // against the length seen here. The engine never writes a pak in place: the writer and
        // reloads replace a pak by rename, which leaves this mapping on the old inode. Against
        // another process: on Windows the handle denies other writers (`open_for_mapping`) and
        // the OS refuses to truncate a mapped file; on POSIX nothing can enforce it, so every read
        // `fstat`s the handle and copies through it once the file changed or while the zero-copy
        // gate is closed (`stored`). The residue — a view served earlier, then the file cut or
        // rewritten in place by another process — is the trade-off of zero-copy loading, stated
        // in the module docs (ADR-177 / ADR-285 accept it; never cut or overwrite a mounted pak).
        #[allow(unsafe_code)]
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let opened = file.metadata()?.modified().ok();
        let mut reader = Self::parse(PakBytes::Mapped(mmap), &path.display().to_string())?;
        reader.opened = Some((file_len, opened));
        reader.file = Some(file);
        Ok(reader)
    }

    /// Whether payloads are served as views of the mapping now: always for owned bytes; for a
    /// mapped pak, while its [`ZeroCopyGate`] is open.
    ///
    /// A mapped pak whose file changed on disk since it was opened (a different length, or a new
    /// modification time) is detected here by an `fstat` of the open handle (about 1 µs); it
    /// warns once and from then on never serves views, so a truncated entry is a
    /// [`PakError::Corrupt`] instead of a `SIGBUS`. Views served earlier stay exposed.
    #[must_use]
    pub fn zero_copy(&self) -> bool {
        self.file_for_reads().is_none()
    }

    /// The file handle to read through, or `None` to borrow the mapping: the handle while the
    /// gate is closed or the file changed on disk.
    fn file_for_reads(&self) -> Option<&std::fs::File> {
        use std::sync::atomic::Ordering;
        let file = self.file.as_ref()?;
        if !self.changed.load(Ordering::Acquire)
            && let (Some((len, mtime)), Ok(now)) = (self.opened, file.metadata())
        {
            let moved = now.len() != len || now.modified().ok() != mtime;
            if moved && !self.changed.swap(true, Ordering::AcqRel) {
                tracing::warn!(
                    target: "regia_asset",
                    path = %self.path,
                    "pak changed on disk while open: reading through the file instead of the mapping"
                );
            }
        }
        (self.changed.load(Ordering::Acquire) || !self.zero_copy.is_open()).then_some(file)
    }

    /// Opens or closes this pak's [`ZeroCopyGate`].
    pub fn set_zero_copy(&self, on: bool) {
        if on {
            self.zero_copy.open();
        } else {
            self.zero_copy.close();
        }
    }

    /// A handle to this pak's [`ZeroCopyGate`] (for the integrity check that opens it).
    #[must_use]
    pub fn zero_copy_gate(&self) -> ZeroCopyGate {
        self.zero_copy.clone()
    }

    /// Parse a pak from in-memory bytes (used on Android where the asset is
    /// read into memory by `regia_platform`).
    pub fn from_bytes(data: Vec<u8>, path: &str, _file_len: u64) -> Result<Self, PakError> {
        Self::parse(PakBytes::Owned(data), path)
    }

    /// Whether the pak is served from a file mapping (no resident copy of the file).
    #[must_use]
    pub fn is_mapped(&self) -> bool {
        matches!(self.data, PakBytes::Mapped(_))
    }

    /// Size of the pak in bytes.
    #[must_use]
    pub fn len_bytes(&self) -> u64 {
        self.data.len() as u64
    }

    /// The TOC entry of `id`.
    #[must_use]
    pub fn entry(&self, id: AssetId) -> Option<&PakEntry> {
        self.index.get(&id).map(|&i| &self.toc.entries[i])
    }

    /// Hint the OS to start reading `id`'s stored bytes (`madvise(WILLNEED)` on a mapped pak).
    ///
    /// Streaming calls this for the next assets of a load set so the page cache fills while the
    /// current asset decodes. A no-op for owned bytes and on non-Unix platforms. Returns the
    /// number of stored bytes the hint covers.
    pub fn prefetch(&self, id: AssetId) -> Result<u64, PakError> {
        let entry = self.entry(id).ok_or(PakError::NotFound(id))?;
        let range = entry_range(entry, self.data.len())?;
        #[cfg(unix)]
        if let PakBytes::Mapped(map) = &self.data
            && !range.is_empty()
        {
            // Advice only: a failure leaves the read path unchanged.
            let _ = map.advise_range(memmap2::Advice::WillNeed, range.start, range.len());
        }
        Ok(range.len() as u64)
    }

    fn parse(data: PakBytes, path: &str) -> Result<Self, PakError> {
        if data.len() < 60 {
            return Err(PakError::BadMagic);
        }
        let header = PakHeader::from_bytes(&data)?;

        // Verify content hash (skip for now — Tier 2 feature).
        let _content_hash = header.content_hash;

        // Read TOC (checked arithmetic: a hostile header must not overflow or overrun the file).
        let toc_oob = || PakError::TocOutOfBounds {
            toc_offset: header.toc_offset,
            toc_len: header.toc_len,
            file_len: data.len() as u64,
        };
        let toc_start = usize::try_from(header.toc_offset).map_err(|_| toc_oob())?;
        let toc_len = usize::try_from(header.toc_len).map_err(|_| toc_oob())?;
        let toc_end = toc_start.checked_add(toc_len).ok_or_else(toc_oob)?;
        if toc_end > data.len() {
            return Err(toc_oob());
        }
        let mut toc: PakToc = postcard::from_bytes(&data[toc_start..toc_end]).map_err(PakError::TocDecode)?;
        crate::manifest::rebrand_type_paths(&mut toc.manifest);

        // Validate all entries are in bounds (offset + size arithmetic is checked so a hostile
        // TOC cannot overflow or reach past the end of the file).
        for entry in &toc.entries {
            let end = entry
                .offset
                .checked_add(entry.compressed_size)
                .ok_or(PakError::OutOfBounds {
                    id: entry.id,
                    offset: entry.offset,
                    size: entry.compressed_size,
                    file_len: header.toc_offset,
                })?;
            if end > header.toc_offset || entry.offset > header.toc_offset {
                return Err(PakError::OutOfBounds {
                    id: entry.id,
                    offset: entry.offset,
                    size: entry.compressed_size,
                    file_len: header.toc_offset,
                });
            }
        }
        // First entry wins on a duplicated id, matching the linear search this index replaced.
        let mut index = std::collections::HashMap::with_capacity(toc.entries.len());
        for (i, entry) in toc.entries.iter().enumerate() {
            index.entry(entry.id).or_insert(i);
        }

        Ok(Self {
            data,
            file: None,
            zero_copy: ZeroCopyGate::new(true),
            opened: None,
            changed: std::sync::atomic::AtomicBool::new(false),
            index,
            max_entry_bytes: FPAK_MAX_ENTRY_BYTES,
            toc,
            header,
            path: path.to_string(),
        })
    }

    /// The stored bytes of `entry` (`range`, validated): borrowed from the pak's bytes, or read
    /// through the file handle while the zero-copy gate is closed, so a file truncated under
    /// the game is an error here instead of a `SIGBUS` on the mapping.
    fn stored(&self, entry: &PakEntry, range: std::ops::Range<usize>) -> Result<Cow<'_, [u8]>, PakError> {
        match self.file_for_reads() {
            Some(file) => read_file_at(file, range.start as u64, range.len())
                .map(Cow::Owned)
                .map_err(|e| PakError::Corrupt {
                    path: self.path.clone(),
                    id: entry.id,
                    offset: entry.offset,
                    reason: format!("stored bytes unreadable ({e}): the file was truncated or changed"),
                }),
            _ => Ok(Cow::Borrowed(&self.data[range])),
        }
    }

    /// Sets the largest output one compressed entry may declare ([`FPAK_MAX_ENTRY_BYTES`] by default).
    /// Past it [`Self::read_decompressed`] answers [`PakError::Corrupt`] before allocating anything.
    pub fn set_max_entry_bytes(&mut self, max: u64) {
        self.max_entry_bytes = max;
    }

    /// Read the raw (possibly compressed) stored bytes for an asset.
    ///
    /// Goes through the same check as every other read: a borrow of the mapping while the
    /// [`ZeroCopyGate`] is open and the file is unchanged on disk, otherwise a copy read through
    /// the file handle, so a pak truncated under the reader is a [`PakError::Corrupt`], never a
    /// `SIGBUS`.
    pub fn read_raw(&self, id: AssetId) -> Result<Cow<'_, [u8]>, PakError> {
        let result = self.read_raw_inner(id);
        crate::access_trace::note_pak(&self.path, id, &result);
        result
    }

    fn read_raw_inner(&self, id: AssetId) -> Result<Cow<'_, [u8]>, PakError> {
        let entry = self.entry(id).ok_or(PakError::NotFound(id))?;
        let range = entry_range(entry, self.data.len())?;
        self.stored(entry, range)
    }

    /// Read and decompress an asset's payload (if compressed).
    ///
    /// An entry is compressed exactly when it has a block table (`block_table_offset != 0`; the
    /// writer puts the table at the entry's offset). `size == compressed_size` is *not* a marker:
    /// a compressed entry's block table plus lz4 blocks can store exactly as many bytes as it
    /// decompresses to, and reading that as raw returned the block table instead of the asset
    /// (the 2026-10-08 gate's `Cooked(BadMagic)`). Corrupt data is a [`PakError::Corrupt`] naming
    /// the entry and its offset, and the output is always exactly `size` bytes.
    ///
    /// An entry of many lz4 blocks decompresses on the compute pool: do not call this (or
    /// [`Self::read_shared`]) while holding a lock that another compute task may take (ADR-303).
    pub fn read_decompressed(&self, id: AssetId) -> Result<Vec<u8>, PakError> {
        let result = self.read_decompressed_inner(id);
        crate::access_trace::note_pak(&self.path, id, &result);
        result
    }

    fn read_decompressed_inner(&self, id: AssetId) -> Result<Vec<u8>, PakError> {
        let entry = self.entry(id).ok_or(PakError::NotFound(id))?;
        if let Some(range) = self.uncompressed_range(id)? {
            return self.stored(entry, range).map(Cow::into_owned);
        }
        let raw = self.stored(entry, entry_range(entry, self.data.len())?)?;
        let corrupt = |reason: String| PakError::Corrupt {
            path: self.path.clone(),
            id,
            offset: entry.offset,
            reason,
        };
        if entry.block_table_offset != entry.offset {
            return Err(corrupt(format!(
                "block table at {} is not at the entry start",
                entry.block_table_offset
            )));
        }
        if entry.size > self.max_entry_bytes {
            return Err(corrupt(format!(
                "declares {} bytes of output, over this reader's limit of {}",
                entry.size, self.max_entry_bytes
            )));
        }
        let size = usize::try_from(entry.size).map_err(|_| out_of_bounds(entry, self.data.len()))?;
        let num_blocks = size.div_ceil(FPAK_COMPRESS_BLOCK_SIZE);
        // The block-offset table (one u32 per block) must fit in the stored bytes, so the number
        // of blocks — and therefore the total output — is bounded by the stored data, never by the
        // attacker-declared `size` alone.
        let table_len = num_blocks
            .checked_mul(4)
            .ok_or_else(|| out_of_bounds(entry, self.data.len()))?;
        if table_len > raw.len() {
            return Err(corrupt(format!(
                "block offset table ({num_blocks} blocks) overruns the {} stored bytes",
                raw.len()
            )));
        }
        let table_bytes = &raw[..table_len];
        let compressed_area = &raw[table_len..];

        // Reject a decompression bomb before allocating any output beyond one block: the summed
        // declared (prepended) uncompressed length of every block must not exceed
        // [`FPAK_MAX_BLOCK_RATIO`] × the stored compressed bytes. Each block is individually capped
        // (see [`decompress_block_into`]), but `N` such blocks would still inflate to `N ×
        // [`FPAK_COMPRESS_BLOCK_SIZE`]`. This pre-pass also validates every block range up front,
        // so a hostile offset table is rejected before any decompression runs.
        let mut declared_total = 0usize;
        for i in 0..num_blocks {
            let compressed = block_slice(table_bytes, compressed_area, i, num_blocks).map_err(corrupt)?;
            let declared = prepended_len(compressed, i).map_err(corrupt)?;
            declared_total = declared_total.saturating_add(declared);
        }
        if declared_total > compressed_area.len().saturating_mul(FPAK_MAX_BLOCK_RATIO) {
            return Err(corrupt(format!(
                "declared output {declared_total} bytes exceeds {FPAK_MAX_BLOCK_RATIO}x the {} stored compressed bytes",
                compressed_area.len()
            )));
        }
        if declared_total != size {
            return Err(corrupt(format!(
                "blocks declare {declared_total} bytes, the entry {size}"
            )));
        }

        // `declared_total == size` and the ratio cap above bound the allocation by the stored data.
        // Every block but the last holds exactly one full block of output, so block `i`
        // decompresses straight into `out[i * BLOCK..]`. A zeroed allocation this large is fresh
        // zero pages from the OS (no memset); the decompressing threads fault them in.
        let mut out = vec![0u8; size];
        let decode = |first: usize, chunk: &mut [u8]| -> Result<(), String> {
            for (j, dst) in chunk.chunks_mut(FPAK_COMPRESS_BLOCK_SIZE).enumerate() {
                let i = first + j;
                let compressed = block_slice(table_bytes, compressed_area, i, num_blocks)?;
                decompress_block_into(compressed, i, dst)?;
            }
            Ok(())
        };
        if num_blocks >= PARALLEL_LZ4_MIN_BLOCKS {
            let _span = tracing::info_span!("pak_lz4_parallel", blocks = num_blocks).entered();
            regia_core::tasks::par_chunks_mut(
                &mut out,
                FPAK_COMPRESS_BLOCK_SIZE * LZ4_BLOCKS_PER_TASK,
                |task, chunk| decode(task * LZ4_BLOCKS_PER_TASK, chunk),
            )
            .into_iter()
            .collect::<Result<(), String>>()
            .map_err(corrupt)?;
        } else {
            decode(0, &mut out).map_err(corrupt)?;
        }
        Ok(out)
    }

    /// [`Self::read_decompressed`] without copying an entry stored uncompressed.
    ///
    /// An uncompressed entry (BC7/KTX2 texture payloads, which lz4 cannot shrink) is served as a
    /// mapped [`PakPayload`] (a range of this pak's bytes): the payload holds an `Arc` to the
    /// reader, so the mapping outlives every borrow even when the caller drops its own handle or a
    /// reload mounts a replacement pak. A compressed entry decompresses into an owned buffer
    /// exactly as [`Self::read_decompressed`] does.
    ///
    /// # Errors
    ///
    /// The same as [`Self::read_decompressed`].
    ///
    /// While the pak's [`ZeroCopyGate`] is closed, an uncompressed entry is copied through the
    /// file handle instead (an owned payload; see the module docs).
    pub fn read_shared(self: &Arc<Self>, id: AssetId) -> Result<PakPayload, PakError> {
        let result = self.read_shared_inner(id);
        crate::access_trace::note_pak(&self.path, id, &result);
        result
    }

    fn read_shared_inner(self: &Arc<Self>, id: AssetId) -> Result<PakPayload, PakError> {
        match self.uncompressed_range(id)? {
            Some(range) if self.zero_copy() => Ok(PakPayload {
                owner: PayloadOwner::Mapped(Arc::clone(self)),
                range,
            }),
            Some(_) | None => self.read_decompressed_inner(id).map(PakPayload::from),
        }
    }

    /// The validated stored range of `id` when it is stored uncompressed (no block table), or
    /// `None` for a compressed entry.
    fn uncompressed_range(&self, id: AssetId) -> Result<Option<std::ops::Range<usize>>, PakError> {
        let entry = self.entry(id).ok_or(PakError::NotFound(id))?;
        if entry.block_table_offset != 0 {
            return Ok(None);
        }
        if entry.compressed_size != entry.size {
            return Err(PakError::Corrupt {
                path: self.path.clone(),
                id,
                offset: entry.offset,
                reason: format!(
                    "stored uncompressed but declares {} bytes over {} stored",
                    entry.size, entry.compressed_size
                ),
            });
        }
        entry_range(entry, self.data.len()).map(Some)
    }

    /// Load the path ↔ id manifest from this pak's TOC.
    pub fn manifest(&self) -> &[ManifestEntry] {
        &self.toc.manifest
    }

    /// Check whether an asset id exists in this pak.
    pub fn contains(&self, id: AssetId) -> bool {
        self.index.contains_key(&id)
    }
}

/// Shared, immutable bytes: a range of a pak's own bytes (an entry stored uncompressed, read by
/// [`PakReader::read_shared`] without a copy) or of an owned buffer (a decompressed entry, a
/// cooked-cache file).
///
/// Dereferences to the bytes. Cloning and [`Self::slice`] / [`Self::subslice`] share the owner
/// (an `Arc` bump, never a copy), so a cooked texture and every mip level cut from it point into
/// the same mapping. A mapped payload keeps its [`PakReader`] — and so the file mapping — alive
/// until the last view of it drops. Serializes exactly like a `Vec<u8>`.
#[derive(Clone)]
pub struct PakPayload {
    owner: PayloadOwner,
    range: std::ops::Range<usize>,
}

#[derive(Clone)]
enum PayloadOwner {
    /// An owned buffer.
    Owned(Arc<Vec<u8>>),
    /// The pak the bytes live in (the mapped file on desktop).
    Mapped(Arc<PakReader>),
}

impl PakPayload {
    /// No bytes. Assigning it over a view drops that view's share of its owner, the way
    /// `*level = Vec::new()` freed an owned level: `*level = PakPayload::empty()` (or
    /// `PakPayload::default()`).
    #[must_use]
    pub fn empty() -> Self {
        Self::from(Vec::new())
    }

    /// Whether the bytes are served from a pak itself (no copy).
    #[must_use]
    pub fn is_mapped(&self) -> bool {
        matches!(self.owner, PayloadOwner::Mapped(_))
    }

    /// The bytes as an owned buffer: moved out when this is the only view of a whole owned
    /// buffer, copied otherwise.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        match self.owner {
            PayloadOwner::Owned(buffer) if self.range == (0..buffer.len()) => {
                Arc::try_unwrap(buffer).unwrap_or_else(|shared| shared[self.range.clone()].to_vec())
            }
            PayloadOwner::Owned(buffer) => buffer[self.range].to_vec(),
            PayloadOwner::Mapped(pak) => pak.data[self.range].to_vec(),
        }
    }

    /// `range` of these bytes (relative to them) as a view sharing the same owner, or `None`
    /// when it is out of bounds.
    #[must_use]
    pub fn slice(&self, range: std::ops::Range<usize>) -> Option<Self> {
        if range.start > range.end || range.end > self.range.len() {
            return None;
        }
        Some(Self {
            owner: self.owner.clone(),
            range: self.range.start + range.start..self.range.start + range.end,
        })
    }

    /// The view of `inner` — a slice borrowed from these very bytes (a field a decoder borrowed
    /// out of them) — sharing the same owner, or `None` when `inner` does not lie inside them.
    #[must_use]
    pub fn subslice(&self, inner: &[u8]) -> Option<Self> {
        let outer = self.as_ptr() as usize;
        let at = (inner.as_ptr() as usize).checked_sub(outer)?;
        self.slice(at..at.checked_add(inner.len())?)
    }
}

impl Default for PakPayload {
    fn default() -> Self {
        Self::empty()
    }
}

impl From<Vec<u8>> for PakPayload {
    fn from(bytes: Vec<u8>) -> Self {
        let range = 0..bytes.len();
        Self {
            owner: PayloadOwner::Owned(Arc::new(bytes)),
            range,
        }
    }
}

impl From<&[u8]> for PakPayload {
    fn from(bytes: &[u8]) -> Self {
        Self::from(bytes.to_vec())
    }
}

impl std::ops::Deref for PakPayload {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.owner {
            PayloadOwner::Owned(buffer) => &buffer[self.range.clone()],
            PayloadOwner::Mapped(pak) => &pak.data[self.range.clone()],
        }
    }
}

impl AsRef<[u8]> for PakPayload {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl std::borrow::Borrow<[u8]> for PakPayload {
    fn borrow(&self) -> &[u8] {
        self
    }
}

impl std::fmt::Debug for PakPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.owner {
            PayloadOwner::Owned(_) => f.debug_struct("Owned").field("len", &self.range.len()).finish(),
            PayloadOwner::Mapped(pak) => f
                .debug_struct("Mapped")
                .field("pak", &pak.path)
                .field("range", &self.range)
                .finish(),
        }
    }
}

impl PartialEq for PakPayload {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for PakPayload {}

impl std::hash::Hash for PakPayload {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl PartialEq<[u8]> for PakPayload {
    fn eq(&self, other: &[u8]) -> bool {
        **self == *other
    }
}

impl PartialEq<&[u8]> for PakPayload {
    fn eq(&self, other: &&[u8]) -> bool {
        **self == **other
    }
}

impl PartialEq<Vec<u8>> for PakPayload {
    fn eq(&self, other: &Vec<u8>) -> bool {
        **self == **other
    }
}

impl PartialEq<PakPayload> for Vec<u8> {
    fn eq(&self, other: &PakPayload) -> bool {
        **self == **other
    }
}

impl PartialEq<PakPayload> for [u8] {
    fn eq(&self, other: &PakPayload) -> bool {
        *self == **other
    }
}

impl Serialize for PakPayload {
    /// The same encoding as the `Vec<u8>` it replaces (a sequence of bytes), so cooked files
    /// written before and after are identical.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (**self).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PakPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<u8>::deserialize(deserializer).map(Self::from)
    }
}

/// Decompress one size-prepended lz4 block into `dst`, which must be exactly the block's output.
///
/// [`prepended_len`] rejects a prepended (little-endian `u32`) uncompressed length above
/// [`FPAK_COMPRESS_BLOCK_SIZE`], and a length other than `dst.len()` is an error before any
/// decoding: the output buffer was sized from the entry, never from a block's own claim, so a
/// hostile block cannot make the reader allocate. The writer never emits a block larger than one
/// [`FPAK_COMPRESS_BLOCK_SIZE`] chunk.
fn decompress_block_into(compressed: &[u8], block_index: usize, dst: &mut [u8]) -> Result<(), String> {
    let declared = prepended_len(compressed, block_index)?;
    if declared != dst.len() {
        return Err(format!(
            "block {block_index} decompressed to {declared} bytes, expected {}",
            dst.len()
        ));
    }
    let written = lz4_flex::block::decompress_into(&compressed[4..], dst).map_err(|e| e.to_string())?;
    if written != dst.len() {
        return Err(format!(
            "block {block_index} decompressed to {written} bytes, expected {}",
            dst.len()
        ));
    }
    Ok(())
}

/// `FILE_SHARE_READ | FILE_SHARE_DELETE` (Win32 `CreateFileW` share flags): others may read,
/// rename or delete a mapped pak, but not open it for writing.
#[cfg(windows)]
const PAK_SHARE_MODE: u32 = 0x1 | 0x4;

/// Opens a pak read-only for [`PakReader::open`]. On Windows the handle withholds
/// `FILE_SHARE_WRITE`, so no other process can open the file for writing while it is mapped (the
/// share mode still allows reading, renaming and deleting it, as the default one did); POSIX has
/// no mandatory write lock, so elsewhere this is a plain read-only open.
fn open_for_mapping(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        options.share_mode(PAK_SHARE_MODE);
    }
    options.open(path)
}

/// Reads exactly `len` bytes at `offset` through `file` (no mapping): a short file is an
/// `UnexpectedEof` error.
fn read_file_at(file: &std::fs::File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    // The table-of-contents length is not trusted: a file truncated under us must fail here,
    // before an allocation sized by an entry that is no longer on disk.
    let on_disk = file.metadata()?.len();
    let end = offset
        .checked_add(len as u64)
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    if end > on_disk {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let mut buf = vec![0u8; len];
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt as _;
        file.read_exact_at(&mut buf, offset)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt as _;
        let mut done = 0;
        while done < len {
            match file.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, offset, &mut buf);
        Err(io::ErrorKind::Unsupported.into())
    }
    #[cfg(any(unix, windows))]
    Ok(buf)
}

/// Read a block's prepended (little-endian `u32`) uncompressed length and enforce the per-block
/// cap. Shared by [`read_decompressed`]'s pre-pass and [`decompress_block_into`] so both reject a
/// hostile length with the same message, before any allocation.
fn prepended_len(compressed: &[u8], block_index: usize) -> Result<usize, String> {
    let declared = match compressed.get(..4) {
        Some(b) => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize,
        None => {
            return Err(format!(
                "block {block_index} is too short to hold a prepended length"
            ));
        }
    };
    if declared > FPAK_COMPRESS_BLOCK_SIZE {
        return Err(format!(
            "block {block_index} declares {declared} bytes, exceeding the {FPAK_COMPRESS_BLOCK_SIZE}-byte block cap"
        ));
    }
    Ok(declared)
}

/// The in-bounds byte range of block `i` within `compressed_area`, read from the block-offset
/// table. Returns an error (rather than panicking) when the offsets point outside the stored data.
fn block_slice<'a>(
    table_bytes: &[u8],
    compressed_area: &'a [u8],
    i: usize,
    num_blocks: usize,
) -> Result<&'a [u8], String> {
    let read = |idx: usize| {
        let base = idx * 4;
        u32::from_le_bytes([
            table_bytes[base],
            table_bytes[base + 1],
            table_bytes[base + 2],
            table_bytes[base + 3],
        ]) as usize
    };
    let block_start = read(i);
    let next_start = if i + 1 < num_blocks {
        read(i + 1)
    } else {
        compressed_area.len()
    };
    compressed_area
        .get(block_start..next_start)
        .ok_or_else(|| format!("block {i} range [{block_start}..{next_start}) overruns the stored data"))
}

/// The in-bounds byte range of an entry within the mapped file, or an [`PakError::OutOfBounds`].
fn entry_range(entry: &PakEntry, data_len: usize) -> Result<std::ops::Range<usize>, PakError> {
    let start = usize::try_from(entry.offset).map_err(|_| out_of_bounds(entry, data_len))?;
    let len = usize::try_from(entry.compressed_size).map_err(|_| out_of_bounds(entry, data_len))?;
    let end = start
        .checked_add(len)
        .ok_or_else(|| out_of_bounds(entry, data_len))?;
    if end > data_len {
        return Err(out_of_bounds(entry, data_len));
    }
    Ok(start..end)
}

/// A [`PakError::OutOfBounds`] for a corrupt or hostile entry.
fn out_of_bounds(entry: &PakEntry, data_len: usize) -> PakError {
    PakError::OutOfBounds {
        id: entry.id,
        offset: entry.offset,
        size: entry.compressed_size,
        file_len: data_len as u64,
    }
}

/// Build the 60-byte pak header for writing.
///
/// A thin wrapper over [`PakHeader::encode`]: it assembles a [`PakHeader`] from the individual
/// fields and writes it, so the write path shares the parser's single header struct (and its
/// fingerprint) rather than duplicating the layout.
pub fn encode_pak_header(
    version: u32,
    flags: u32,
    toc_offset: u64,
    toc_len: u64,
    content_hash: &[u8; 32],
) -> [u8; 60] {
    PakHeader {
        flags,
        toc_offset,
        toc_len,
        content_hash: *content_hash,
    }
    .encode(version)
}

/// Rebuild a pak file's bytes with a replacement TOC (the content hash is not verified in v1).
#[cfg(test)]
fn rebuild_with_toc(original: &[u8], toc: &PakToc) -> Vec<u8> {
    let header = PakHeader::from_bytes(original).expect("header");
    let mut out = original[..header.toc_offset as usize].to_vec();
    out.extend_from_slice(&postcard::to_allocvec(toc).expect("toc"));
    let header = encode_pak_header(
        FPAK_VERSION,
        header.flags,
        header.toc_offset,
        (out.len() - header.toc_offset as usize) as u64,
        &header.content_hash,
    );
    out[..60].copy_from_slice(&header);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// audit-v035-formats 14: header flag bits were never validated, so a v1 pak with
    /// `TOC_COMPRESSED` (or an unknown bit) was decoded as if the TOC were plain postcard.
    #[test]
    fn a_header_with_flag_bits_this_runtime_does_not_read_is_refused() {
        let header = |flags: u32| encode_pak_header(FPAK_VERSION, flags, 60, 0, &[0u8; 32]);
        assert!(PakHeader::from_bytes(&header(0)).is_ok());
        assert!(PakHeader::from_bytes(&header(flags::COMPRESSED)).is_ok());
        for bad in [
            flags::TOC_COMPRESSED,
            flags::COMPRESSED | flags::TOC_COMPRESSED,
            1 << 7,
        ] {
            let err = PakHeader::from_bytes(&header(bad)).expect_err("an unread flag is refused");
            assert!(err.to_string().contains("flags"), "{bad:#x}: {err}");
        }
    }

    fn make_test_pak(entries: Vec<(AssetId, Vec<u8>)>, compressed: bool) -> Vec<u8> {
        let mut data = Vec::new();
        let mut toc_entries = Vec::new();

        // Write header placeholder (60 bytes).
        let header_pos = data.len();
        data.extend_from_slice(&[0u8; 60]);

        for (id, payload) in &entries {
            // Align to 4096.
            let rem = data.len() % FPAK_BLOB_ALIGNMENT as usize;
            if rem != 0 {
                data.resize(data.len() + (FPAK_BLOB_ALIGNMENT as usize - rem), 0);
            }
            let offset = data.len() as u64;
            let size = payload.len() as u64;

            if compressed && !payload.is_empty() {
                // Compress in blocks.
                let mut compressed_blocks = Vec::new();
                let mut block_offsets = Vec::new();
                let mut raw_offset = 0u32;

                for chunk in payload.chunks(FPAK_COMPRESS_BLOCK_SIZE) {
                    block_offsets.push(raw_offset);
                    let compressed_block = lz4_flex::compress_prepend_size(chunk);
                    raw_offset += compressed_block.len() as u32;
                    compressed_blocks.extend_from_slice(&compressed_block);
                }
                // Write block offset table right before the compressed data.
                let block_table_offset = data.len() as u64;
                for off in &block_offsets {
                    data.extend_from_slice(&off.to_le_bytes());
                }
                let data_start = data.len();
                data.extend_from_slice(&compressed_blocks);

                toc_entries.push(PakEntry {
                    id: *id,
                    type_tag: TypePathHash(0),
                    offset,
                    size,
                    compressed_size: (data_start - offset as usize + compressed_blocks.len()) as u64,
                    block_table_offset,
                });
            } else {
                data.extend_from_slice(payload);
                toc_entries.push(PakEntry {
                    id: *id,
                    type_tag: TypePathHash(0),
                    offset,
                    size,
                    compressed_size: size,
                    block_table_offset: 0,
                });
            }
        }

        let toc_offset = data.len() as u64;
        let toc = PakToc {
            entries: toc_entries,
            manifest: Vec::new(),
            dependencies: Vec::new(),
        };
        let toc_bytes = postcard::to_allocvec(&toc).expect("toc encode");
        let toc_len = toc_bytes.len() as u64;
        data.extend_from_slice(&toc_bytes);

        // Hash the content (everything before TOC).
        let content_hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();

        let flags = if compressed { flags::COMPRESSED } else { 0 };
        let header = encode_pak_header(FPAK_VERSION, flags, toc_offset, toc_len, &content_hash);
        data[header_pos..header_pos + 60].copy_from_slice(&header);

        data
    }

    /// Deterministic pseudo-random bytes (a 64-bit truncated LCG): incompressible enough that the
    /// round-trip fixtures stay below [`FPAK_MAX_BLOCK_RATIO`] and exercise the real decompression
    /// path, unlike a repetitive pattern (which lz4 collapses to ~250:1 and the ratio cap rejects).
    fn randomish_bytes(len: usize) -> Vec<u8> {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push((state >> 33) as u8);
        }
        out
    }

    #[test]
    fn round_trip_uncompressed() {
        let id = AssetId::new_random();
        let payload = b"hello regia pak".to_vec();
        let bytes = make_test_pak(vec![(id, payload.clone())], false);
        let len = bytes.len() as u64;

        let reader = PakReader::from_bytes(bytes, "test.fpak", len).expect("open");
        assert_eq!(reader.toc.entries.len(), 1);
        assert_eq!(reader.toc.entries[0].id, id);
        assert!(!reader.header.is_compressed());

        let read = reader.read_decompressed(id).expect("read");
        assert_eq!(read, payload);
    }

    #[test]
    fn open_maps_the_file_without_copying_it() {
        let a = AssetId::new_random();
        let b = AssetId::new_random();
        let payload_a = randomish_bytes(300_000);
        let payload_b = b"second".to_vec();
        let bytes = make_test_pak(vec![(a, payload_a.clone()), (b, payload_b.clone())], true);
        let dir = std::env::temp_dir().join(format!("regia-pak-mmap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("mapped.fpak");
        std::fs::write(&path, &bytes).expect("write pak");

        let reader = PakReader::open(&path).expect("open");
        assert!(reader.is_mapped(), "desktop paks are served from the mapping");
        assert_eq!(reader.len_bytes(), bytes.len() as u64);
        assert_eq!(reader.read_decompressed(a).expect("read a"), payload_a);
        assert_eq!(reader.read_decompressed(b).expect("read b"), payload_b);
        assert_eq!(reader.entry(b).map(|e| e.size), Some(payload_b.len() as u64));
        assert!(reader.prefetch(a).expect("prefetch") > 0);
        assert!(matches!(
            reader.prefetch(AssetId::new_random()),
            Err(PakError::NotFound(_))
        ));
        // Owned bytes (Android) take the same read path.
        let owned = PakReader::from_bytes(bytes.clone(), "owned.fpak", bytes.len() as u64).expect("owned");
        assert!(!owned.is_mapped());
        assert_eq!(owned.read_decompressed(a).expect("owned read"), payload_a);
        drop(reader);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_compressed() {
        let id = AssetId::new_random();
        // Payload large enough to need compression blocks, but incompressible enough to stay
        // under the decompression-bomb ratio cap.
        let payload: Vec<u8> = randomish_bytes(100_000);
        let pak_bytes = make_test_pak(vec![(id, payload.clone())], true);

        let reader =
            PakReader::from_bytes(pak_bytes.clone(), "test.fpak", pak_bytes.len() as u64).expect("open");
        assert!(reader.header.is_compressed());

        let read = reader.read_decompressed(id).expect("read");
        assert_eq!(read, payload);
    }

    #[test]
    fn multiple_entries() {
        let id1 = AssetId::new_random();
        let id2 = AssetId::new_random();
        let id3 = AssetId::new_random();
        let payloads = [
            (id1, b"asset one".to_vec()),
            (id2, b"asset two is longer".to_vec()),
            (id3, b"three".to_vec()),
        ];
        let pak_bytes = make_test_pak(payloads.to_vec(), false);

        let reader =
            PakReader::from_bytes(pak_bytes.clone(), "test.fpak", pak_bytes.len() as u64).expect("open");
        assert_eq!(reader.toc.entries.len(), 3);
        assert!(reader.contains(id1));
        assert!(reader.contains(id2));
        assert!(reader.contains(id3));
        assert!(!reader.contains(AssetId::new_random()));

        assert_eq!(reader.read_decompressed(id1).expect("read"), b"asset one");
        assert_eq!(
            reader.read_decompressed(id2).expect("read"),
            b"asset two is longer"
        );
        assert_eq!(reader.read_decompressed(id3).expect("read"), b"three");
    }

    #[test]
    fn bad_magic_rejected() {
        let bytes = vec![0u8; 100];
        let result = PakReader::from_bytes(bytes, "bad.fpak", 100);
        assert!(matches!(result, Err(PakError::BadMagic)));
    }

    #[test]
    fn unknown_version_rejected() {
        let header = encode_pak_header(999, 0, 60, 10, &[0u8; 32]);
        let mut bytes = vec![0u8; 100];
        bytes[..60].copy_from_slice(&header);
        let result = PakReader::from_bytes(bytes, "future.fpak", 100);
        assert!(matches!(result, Err(PakError::UnsupportedVersion { .. })));
    }

    #[test]
    fn older_version_rejected() {
        // A version below the current one is rejected rather than parsed as v1 (a migrator would
        // be needed, and none exists yet).
        let header = encode_pak_header(0, 0, 60, 10, &[0u8; 32]);
        let mut bytes = vec![0u8; 100];
        bytes[..60].copy_from_slice(&header);
        let result = PakReader::from_bytes(bytes, "old.fpak", 100);
        assert!(
            matches!(
                result,
                Err(PakError::UnsupportedVersion {
                    found: 0,
                    supported: 1
                })
            ),
            "older pak version must be rejected with UnsupportedVersion"
        );
    }

    #[test]
    fn not_found_error() {
        let id = AssetId::new_random();
        let pak_bytes = make_test_pak(vec![(id, vec![1, 2, 3])], false);
        let reader =
            PakReader::from_bytes(pak_bytes.clone(), "test.fpak", pak_bytes.len() as u64).expect("open");
        let result = reader.read_decompressed(AssetId::new_random());
        assert!(matches!(result, Err(PakError::NotFound(_))));
    }

    #[test]
    fn manifest_is_read() {
        let id = AssetId::new_random();
        let pak_bytes = make_test_pak(vec![(id, vec![1, 2, 3])], false);
        // Rebuild with manifest by manually creating the pak.
        // For simplicity, test via from_bytes directly.
        let reader =
            PakReader::from_bytes(pak_bytes.clone(), "test.fpak", pak_bytes.len() as u64).expect("open");
        assert_eq!(reader.manifest().len(), 0);
    }

    #[test]
    fn large_asset_compressed() {
        let id = AssetId::new_random();
        // 512 KiB payload to exercise multiple compression blocks (incompressible so it stays
        // under the decompression-bomb ratio cap).
        let payload: Vec<u8> = randomish_bytes(512 * 1024);
        let pak_bytes = make_test_pak(vec![(id, payload.clone())], true);

        let reader =
            PakReader::from_bytes(pak_bytes.clone(), "test.fpak", pak_bytes.len() as u64).expect("open");
        let read = reader.read_decompressed(id).expect("read");
        assert_eq!(read.len(), payload.len());
        assert_eq!(read, payload);
    }

    #[test]
    fn empty_payload() {
        let id = AssetId::new_random();
        let payload = Vec::new();
        let pak_bytes = make_test_pak(vec![(id, payload.clone())], false);
        let reader =
            PakReader::from_bytes(pak_bytes.clone(), "test.fpak", pak_bytes.len() as u64).expect("open");
        let read = reader.read_decompressed(id).expect("read");
        assert!(read.is_empty());
    }

    #[test]
    fn pak_header_round_trip() {
        let header = encode_pak_header(1, flags::COMPRESSED, 4096, 128, &[0xAB; 32]);
        let parsed = PakHeader::from_bytes(&header).expect("parse");
        assert_eq!(parsed.flags, flags::COMPRESSED);
        assert_eq!(parsed.toc_offset, 4096);
        assert_eq!(parsed.toc_len, 128);
        assert_eq!(parsed.content_hash, [0xAB; 32]);
        assert!(parsed.is_compressed());
    }

    #[test]
    fn schema_fingerprint_derives_header_and_toc() {
        // The fingerprint must be derived from the actual structs the parser reads/writes — not a
        // handwritten list (round-1's `"magic,version,flags,toc_offset,toc_len,content_hash"`) nor a
        // second mirror struct kept in sync by hand. Re-derive it from the same types and require
        // exact equality: adding a field to `PakHeader` then changes the hash without editing a
        // second struct, and restoring the round-1 handwritten header (which lists `magic` and
        // `version` and omits `PakEntry`/`ManifestEntry`) no longer matches.
        let expected = regia_core::schema::join_field_lists(&[
            regia_core::schema::serde_field_names::<PakHeader>(),
            regia_core::schema::serde_field_names::<PakToc>(),
            regia_core::schema::serde_field_names::<PakEntry>(),
            regia_core::schema::serde_field_names::<crate::manifest::ManifestEntry>(),
        ]);
        assert_eq!(pak_schema_fingerprint(), expected);
        // `PakHeader`'s shape fields, in declaration order. `magic`/`version` are absent (they are
        // validated constants, not shape), so a revert to the round-1 handwritten header fails.
        assert!(
            expected.starts_with("flags,toc_offset,toc_len,content_hash"),
            "header fingerprint is not derived from PakHeader: {expected}"
        );
        assert!(
            expected.contains("entries"),
            "PakToc not in fingerprint: {expected}"
        );
        assert!(
            expected.contains("compressed_size"),
            "PakEntry not in fingerprint: {expected}"
        );
    }

    /// A compressed pak whose entry declares a `size` larger than its block table can describe
    /// (a decompression bomb / corrupt length field) returns an error instead of allocating.
    #[test]
    fn hostile_size_field_does_not_panic() {
        let id = AssetId::new_random();
        let bytes = make_test_pak(vec![(id, b"payload".to_vec())], true);
        let reader = PakReader::from_bytes(bytes.clone(), "x.fpak", bytes.len() as u64).expect("open");
        let mut toc = reader.toc.clone();
        toc.entries[0].size = 1 << 40; // 1 TiB of claimed output, a few blocks stored
        let bytes = rebuild_with_toc(&bytes, &toc);
        let reader = PakReader::from_bytes(bytes.clone(), "x.fpak", bytes.len() as u64).expect("open");
        assert!(
            reader.read_decompressed(id).is_err(),
            "must reject the hostile size"
        );
    }

    /// `read_decompressed` rejects a block-offset table whose offsets point outside the stored data.
    #[test]
    fn hostile_block_offsets_do_not_panic() {
        let id = AssetId::new_random();
        let bytes = make_test_pak(vec![(id, (0..70_000).map(|i| (i % 251) as u8).collect())], true);
        let reader = PakReader::from_bytes(bytes.clone(), "x.fpak", bytes.len() as u64).expect("open");
        // Corrupt the stored block-offset table in place: it sits at the entry's raw start.
        let entry = reader.toc.entries[0].clone();
        let table_off = entry.offset as usize;
        let mut corrupt = bytes;
        // First block offset (u32 LE) is set far past the end of the compressed area.
        corrupt[table_off..table_off + 4].copy_from_slice(&(u32::MAX).to_le_bytes());
        let reader = PakReader::from_bytes(corrupt.clone(), "x.fpak", corrupt.len() as u64).expect("open");
        assert!(
            reader.read_decompressed(id).is_err(),
            "out-of-range offset must be an error"
        );
    }

    /// A block whose prepended uncompressed length is `u32::MAX` (≈4 GiB) must be rejected before
    /// `lz4_flex::decompress_size_prepended` allocates a buffer that size — the per-block cap at
    /// [`FPAK_COMPRESS_BLOCK_SIZE`] turns it into an error, not an OOM. The error must be the
    /// per-block cap rejection (not a generic lz4 message like "0 is not a valid match offset").
    #[test]
    fn hostile_prepended_block_size_is_rejected() {
        let id = AssetId::new_random();
        let payload: Vec<u8> = randomish_bytes(70_000);
        let mut bytes = make_test_pak(vec![(id, payload)], true);
        let reader = PakReader::from_bytes(bytes.clone(), "x.fpak", bytes.len() as u64).expect("open");
        let entry = reader.toc.entries[0].clone();
        // The compressed block (with its prepended u32) follows the one-entry block-offset table.
        let num_blocks = (entry.size as usize).div_ceil(FPAK_COMPRESS_BLOCK_SIZE);
        let block_off = entry.offset as usize + num_blocks * 4;
        bytes[block_off..block_off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let reader = PakReader::from_bytes(bytes.clone(), "x.fpak", bytes.len() as u64).expect("open");
        let err = reader
            .read_decompressed(id)
            .expect_err("a hostile prepended block size must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeding") && msg.contains(&FPAK_COMPRESS_BLOCK_SIZE.to_string()),
            "the rejection must be the per-block cap, got: {msg}"
        );
    }

    /// A pak of several valid, highly-compressible 256 KiB blocks (a run of zeros stores in ~1043
    /// bytes, ~251:1) must be rejected as a decompression bomb before the output grows to
    /// `N × 256 KiB`: the summed declared size exceeds [`FPAK_MAX_BLOCK_RATIO`] × the stored
    /// compressed bytes.
    #[test]
    fn decompression_bomb_ratio_is_rejected() {
        let id = AssetId::new_random();
        // 4 × 256 KiB of zeros: each block compresses to ~1043 bytes (~251:1), so the summed
        // declared size (~1 MiB) is ~251× the stored compressed bytes — far over the ratio cap.
        let payload: Vec<u8> = vec![0u8; FPAK_COMPRESS_BLOCK_SIZE].repeat(4);
        let pak_bytes = make_test_pak(vec![(id, payload)], true);
        let reader =
            PakReader::from_bytes(pak_bytes.clone(), "x.fpak", pak_bytes.len() as u64).expect("open");
        let err = reader
            .read_decompressed(id)
            .expect_err("a zero-block decompression bomb must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("exceeds") && msg.contains(&FPAK_MAX_BLOCK_RATIO.to_string()),
            "the rejection must be the ratio cap, got: {msg}"
        );
    }

    /// Audit ADR-3206 finding 8: the ratio cap alone lets a 256 MiB pak ask for 16 GiB at once.
    /// A real, intact entry is refused at the limit and read one byte above it.
    #[test]
    fn an_entry_over_the_output_limit_is_refused_before_it_is_decompressed() {
        let id = AssetId::new_random();
        let payload = randomish_bytes(FPAK_COMPRESS_BLOCK_SIZE + 17);
        let pak_bytes = make_test_pak(vec![(id, payload.clone())], true);
        let mut reader =
            PakReader::from_bytes(pak_bytes.clone(), "x.fpak", pak_bytes.len() as u64).expect("open");
        assert_eq!(
            reader.read_decompressed(id).expect("under the default limit"),
            payload
        );
        reader.set_max_entry_bytes(payload.len() as u64 - 1);
        let message = reader
            .read_decompressed(id)
            .expect_err("over the limit")
            .to_string();
        assert!(message.contains("over this reader's limit"), "{message}");
        reader.set_max_entry_bytes(payload.len() as u64);
        assert_eq!(reader.read_decompressed(id).expect("at the limit"), payload);
    }

    /// A payload whose compressed entry (block table + lz4 blocks, the pre-2026-10-08 writer's
    /// layout) stores exactly `size` bytes: noise then a zero run, the run length swept until the
    /// stored length lands on the payload length.
    fn payload_storing_exactly_its_size(len: usize) -> Vec<u8> {
        (16..len.min(4096))
            .map(|zeros| {
                let mut p = randomish_bytes(len);
                p[len - zeros..].fill(0);
                p
            })
            .find(|p| 4 + lz4_flex::compress_prepend_size(p).len() == p.len())
            .expect("a zero-run length lands the stored size on the payload size")
    }

    /// Regression (gate 2026-10-08, `terrain index from the pak: Cooked(BadMagic)`): a compressed
    /// entry whose stored bytes happen to equal its uncompressed size was returned raw — block
    /// table first — because the reader used `size == compressed_size` as the "stored raw" marker.
    #[test]
    fn compressed_entry_storing_exactly_its_size_is_decompressed() {
        for len in [300, 777, 2_048, 20_000] {
            let id = AssetId::new_random();
            let payload = payload_storing_exactly_its_size(len);
            let bytes = make_test_pak(vec![(id, payload.clone())], true);
            let reader = PakReader::from_bytes(bytes.clone(), "x.fpak", bytes.len() as u64).expect("open");
            let entry = reader.entry(id).expect("entry");
            assert_eq!(entry.compressed_size, entry.size, "len {len} hits the boundary");
            assert_eq!(reader.read_decompressed(id).expect("read"), payload, "len {len}");
        }
    }

    /// Corrupt stored data is a [`PakError::Corrupt`] naming the pak, the entry id and its offset —
    /// not bytes that fail later as a bad cooked magic.
    #[test]
    fn corrupt_entries_name_the_entry_and_offset() {
        let id = AssetId::new_random();
        let payload = randomish_bytes(300_000);
        let bytes = make_test_pak(vec![(id, payload)], true);
        let reader = PakReader::from_bytes(bytes.clone(), "c.fpak", bytes.len() as u64).expect("open");
        let entry = reader.entry(id).expect("entry").clone();
        let original_toc = reader.toc.clone();

        // A block that decompresses short of a full block (declared size shrunk by one).
        let mut toc = original_toc.clone();
        toc.entries[0].size -= 1;
        let short = rebuild_with_toc(&bytes, &toc);
        let reader = PakReader::from_bytes(short.clone(), "c.fpak", short.len() as u64).expect("open");
        let err = reader
            .read_decompressed(id)
            .expect_err("size disagrees with the blocks");
        assert!(
            matches!(&err, PakError::Corrupt { id: e, offset, path, .. } if *e == id && *offset == entry.offset && path == "c.fpak"),
            "{err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains(&id.to_string()) && msg.contains(&entry.offset.to_string()),
            "{msg}"
        );

        // A block whose prepended length is one short of a full block, and a block cut short by
        // moving the next block's start (the lz4 format has no checksum, so these are the
        // corruptions the layout itself can catch).
        let table = entry.offset as usize;
        let block0 = table + 2 * 4;
        let mut short_len = bytes.clone();
        short_len[block0..block0 + 4].copy_from_slice(&(FPAK_COMPRESS_BLOCK_SIZE as u32 - 1).to_le_bytes());
        let mut cut_block = bytes.clone();
        let next = u32::from_le_bytes(cut_block[table + 4..table + 8].try_into().expect("u32"));
        cut_block[table + 4..table + 8].copy_from_slice(&(next - 100).to_le_bytes());
        for corrupt in [short_len, cut_block] {
            let reader =
                PakReader::from_bytes(corrupt.clone(), "c.fpak", corrupt.len() as u64).expect("open");
            let result = reader.read_decompressed(id);
            assert!(
                matches!(&result, Err(PakError::Corrupt { id: e, .. }) if *e == id),
                "{:?}",
                result.err()
            );
        }

        // An uncompressed entry whose stored length disagrees with its size.
        let mut toc = original_toc.clone();
        toc.entries[0].block_table_offset = 0;
        let raw_mismatch = rebuild_with_toc(&bytes, &toc);
        let reader =
            PakReader::from_bytes(raw_mismatch.clone(), "c.fpak", raw_mismatch.len() as u64).expect("open");
        assert!(matches!(
            reader.read_decompressed(id),
            Err(PakError::Corrupt { .. })
        ));

        // A block table that is not at the entry start.
        let mut toc = original_toc.clone();
        toc.entries[0].block_table_offset = entry.offset + 4;
        let moved = rebuild_with_toc(&bytes, &toc);
        let reader = PakReader::from_bytes(moved.clone(), "c.fpak", moved.len() as u64).expect("open");
        assert!(matches!(
            reader.read_decompressed(id),
            Err(PakError::Corrupt { .. })
        ));
    }

    /// A pak cut short (still being written, or truncated) names the TOC range and the file length.
    #[test]
    fn truncated_pak_reports_the_toc_range() {
        let id = AssetId::new_random();
        let bytes = make_test_pak(vec![(id, randomish_bytes(10_000))], true);
        let cut = bytes[..bytes.len() - 3].to_vec();
        let len = cut.len() as u64;
        match PakReader::from_bytes(cut, "t.fpak", len) {
            Err(PakError::TocOutOfBounds { file_len, .. }) => assert_eq!(file_len, len),
            other => panic!("expected TocOutOfBounds, got {:?}", other.err()),
        }
    }

    /// The decompression path (`read_decompressed`) is fuzzed over deterministic corruptions of a
    /// valid compressed pak — not just `PakReader::from_bytes` — so a hostile TOC or block that
    /// only manifests during decompression is still caught.
    #[test]
    fn read_decompressed_never_panics() {
        use regia_test_utils::fuzz::fuzz_loader;
        let id = AssetId::new_random();
        let payload: Vec<u8> = randomish_bytes(200_000);
        let pak_bytes = make_test_pak(vec![(id, payload)], true);
        let len = pak_bytes.len() as u64;
        fuzz_loader(&pak_bytes, 0xA4, 3_000, move |bytes| {
            let reader = PakReader::from_bytes(bytes.to_vec(), "fuzz.fpak", len)?;
            for entry in reader.toc.entries.iter() {
                let _ = reader.read_decompressed(entry.id)?;
            }
            Ok::<(), PakError>(())
        });
    }

    /// A unique temp directory for one test (removed by the caller).
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("regia-pak-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn read_shared_serves_a_raw_entry_from_the_mapping_without_copying() {
        let id = AssetId::new_random();
        let payload = randomish_bytes(3 * 4096 + 17);
        let bytes = make_test_pak(vec![(id, payload.clone())], false);
        let dir = temp_dir("shared-raw");
        let path = dir.join("raw.fpak");
        std::fs::write(&path, &bytes).expect("write pak");
        let reader = Arc::new(PakReader::open(&path).expect("open"));
        assert!(reader.is_mapped());

        let shared = reader.read_shared(id).expect("read");
        assert!(shared.is_mapped());
        assert_eq!(shared, payload);
        // Pointer-in-mapping: the payload is the pak's own bytes, not a heap copy.
        let raw = reader.read_raw(id).expect("raw");
        assert_eq!(shared.as_ptr(), raw.as_ptr());
        let map = reader.data.as_ptr_range();
        assert!(map.contains(&shared.as_ptr()));
        // The copying path still copies.
        let copied = reader.read_decompressed(id).expect("read");
        assert_ne!(copied.as_ptr(), raw.as_ptr());
        drop(shared);
        drop(reader);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_raw_shared_payload_from_owned_bytes_outlives_its_reader() {
        let id = AssetId::new_random();
        let payload = randomish_bytes(257);
        let bytes = make_test_pak(vec![(id, payload.clone())], false);
        let reader = Arc::new(PakReader::from_bytes(bytes, "owned.fpak", 0).expect("open"));
        let shared = reader.read_shared(id).expect("shared read");
        assert_eq!(shared, payload);
        drop(reader);
        assert_eq!(shared, payload, "the payload retains its source bytes");
    }

    #[test]
    fn read_shared_decompresses_a_compressed_entry_like_read_decompressed() {
        let id = AssetId::new_random();
        let payload = randomish_bytes(3 * FPAK_COMPRESS_BLOCK_SIZE + 5);
        let bytes = make_test_pak(vec![(id, payload.clone())], true);
        let len = bytes.len() as u64;
        let reader = Arc::new(PakReader::from_bytes(bytes, "c.fpak", len).expect("open"));
        let shared = reader.read_shared(id).expect("read");
        assert!(!shared.is_mapped());
        assert_eq!(shared, payload);
        assert_eq!(shared, reader.read_decompressed(id).expect("read"));
        assert_eq!(shared.into_vec(), payload);
    }

    #[test]
    fn read_shared_rejects_what_read_decompressed_rejects() {
        let id = AssetId::new_random();
        let bytes = make_test_pak(vec![(id, b"payload".to_vec())], false);
        let mut toc = PakReader::from_bytes(bytes.clone(), "t.fpak", 0)
            .expect("open")
            .toc;
        toc.entries[0].size += 1;
        let corrupt = rebuild_with_toc(&bytes, &toc);
        let reader = Arc::new(PakReader::from_bytes(corrupt, "t.fpak", 0).expect("open"));
        assert!(matches!(reader.read_shared(id), Err(PakError::Corrupt { .. })));
        assert!(matches!(
            reader.read_decompressed(id),
            Err(PakError::Corrupt { .. })
        ));
        let missing = AssetId::new_random();
        assert!(matches!(reader.read_shared(missing), Err(PakError::NotFound(_))));
    }

    // Unix only: Windows may refuse to rename over, or delete, a file with a live mapped view
    // (`ERROR_USER_MAPPED_FILE`), which is a property of the OS, not of the payload.
    #[cfg(unix)]
    #[test]
    fn a_mapped_payload_outlives_its_reader_and_a_replaced_file() {
        let id = AssetId::new_random();
        let payload = randomish_bytes(2 * 4096 + 3);
        let dir = temp_dir("shared-outlive");
        let path = dir.join("live.fpak");
        std::fs::write(&path, make_test_pak(vec![(id, payload.clone())], false)).expect("write pak");
        let reader = Arc::new(PakReader::open(&path).expect("open"));
        let shared = reader.read_shared(id).expect("read");
        // The caller drops its handle (an unmount) and a re-cook replaces the file by rename, as
        // the writer and asset reload do: the payload keeps the old mapping and its bytes.
        drop(reader);
        let next = dir.join("live.fpak.tmp");
        std::fs::write(&next, make_test_pak(vec![(id, vec![0u8; 9000])], false)).expect("write pak");
        std::fs::rename(&next, &path).expect("rename");
        assert!(shared.is_mapped());
        assert_eq!(shared, payload);
        let reopened = Arc::new(PakReader::open(&path).expect("reopen"));
        assert_eq!(reopened.read_shared(id).expect("read"), vec![0u8; 9000]);
        // Even with the path gone, the mapping stays valid until the last payload drops.
        std::fs::remove_file(&path).expect("remove");
        assert_eq!(shared, payload);
        drop(shared);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn payload_slices_share_the_owner_and_check_bounds() {
        let id = AssetId::new_random();
        let payload = randomish_bytes(4096 + 100);
        let bytes = make_test_pak(vec![(id, payload.clone())], false);
        let len = bytes.len() as u64;
        let reader = Arc::new(PakReader::from_bytes(bytes, "s.fpak", len).expect("open"));
        let shared = reader.read_shared(id).expect("read");
        let part = shared.slice(10..4000).expect("in bounds");
        assert_eq!(part, payload[10..4000].to_vec());
        assert_eq!(
            part.as_ptr(),
            shared[10..].as_ptr(),
            "a slice is a view, not a copy"
        );
        assert!(part.is_mapped());
        let inner = part.slice(5..6).expect("nested");
        assert_eq!(inner.as_ptr(), shared[15..].as_ptr());
        assert!(shared.slice(0..shared.len() + 1).is_none());
        // A start past the end (computed, so the range is not a constant).
        let start = shared.len().min(9);
        let reversed = shared.slice(start..3);
        assert!(reversed.is_none());
        // `subslice` finds a borrowed field inside the payload and refuses foreign bytes.
        let borrowed = &shared[100..200];
        let view = shared.subslice(borrowed).expect("inside");
        assert_eq!(view.as_ptr(), borrowed.as_ptr());
        assert_eq!(view.len(), 100);
        let foreign = vec![0u8; 10];
        assert!(shared.subslice(&foreign).is_none());
        // Every view keeps the reader alive.
        drop(shared);
        drop(reader);
        assert_eq!(view, payload[100..200].to_vec());
    }

    #[test]
    fn owned_payload_round_trips_without_copying_when_unique() {
        let bytes = randomish_bytes(5000);
        let payload = PakPayload::from(bytes.clone());
        assert!(!payload.is_mapped());
        let moved = payload.into_vec();
        assert_eq!(moved, bytes);
        let payload = PakPayload::from(moved);
        let clone = payload.clone();
        assert_eq!(clone.as_ptr(), payload.as_ptr(), "a clone shares the buffer");
        // Shared or partial views copy out; the original stays intact.
        assert_eq!(clone.into_vec(), bytes);
        assert_eq!(
            payload.slice(1..3).expect("slice").into_vec(),
            bytes[1..3].to_vec()
        );
        assert_eq!(PakPayload::default().len(), 0);
    }

    #[test]
    fn payload_serializes_like_the_vec_it_replaces() {
        let bytes = randomish_bytes(300);
        let as_vec = postcard::to_allocvec(&bytes).expect("vec");
        let payload = PakPayload::from(bytes.clone());
        let as_payload = postcard::to_allocvec(&payload).expect("payload");
        assert_eq!(as_vec, as_payload);
        let back: PakPayload = postcard::from_bytes(&as_vec).expect("decode");
        assert_eq!(back, bytes);
        // A borrowed decode of the same wire bytes points into the input (zero-copy decoders).
        let borrowed: &[u8] = postcard::from_bytes(&as_vec).expect("borrow");
        assert_eq!(borrowed, &bytes[..]);
        assert!(as_vec.as_ptr_range().contains(&borrowed.as_ptr()));
    }

    /// The host migration for code that mutated owned levels in place: clearing a view drops its
    /// share of the owner (the last view frees it), and a `Vec<u8>` assigns with `.into()`.
    #[test]
    fn clearing_and_refilling_a_view_frees_and_moves_like_a_vec() {
        let buffer = PakPayload::from(randomish_bytes(4096));
        let mut levels = vec![
            buffer.slice(0..1024).expect("level 0"),
            buffer.slice(1024..1280).expect("level 1"),
        ];
        let owner = match &buffer.owner {
            PayloadOwner::Owned(arc) => Arc::downgrade(arc),
            PayloadOwner::Mapped(_) => unreachable!("an owned buffer"),
        };
        drop(buffer);
        assert!(owner.upgrade().is_some(), "the levels keep the buffer");
        for level in &mut levels {
            *level = PakPayload::empty();
        }
        assert!(owner.upgrade().is_none(), "clearing every level frees the buffer");
        assert!(levels.iter().all(|l| l.is_empty()));
        let refill = randomish_bytes(64);
        let ptr = refill.as_ptr();
        levels[0] = refill.into();
        assert_eq!(levels[0].as_ptr(), ptr, "a Vec assigns without a copy");
        assert_eq!(PakPayload::default(), PakPayload::empty());
    }

    #[test]
    fn a_large_compressed_entry_decompresses_its_blocks_in_parallel_identically() {
        let id = AssetId::new_random();
        // Above PARALLEL_LZ4_MIN_BLOCKS, with a short last block.
        let payload = randomish_bytes((PARALLEL_LZ4_MIN_BLOCKS + 5) * FPAK_COMPRESS_BLOCK_SIZE + 4321);
        let bytes = make_test_pak(vec![(id, payload.clone())], true);
        let len = bytes.len() as u64;
        let reader = Arc::new(PakReader::from_bytes(bytes, "p.fpak", len).expect("open"));
        assert_eq!(reader.read_decompressed(id).expect("read"), payload);
        assert_eq!(reader.read_shared(id).expect("read"), payload);
    }

    fn error_category(result: &Result<Vec<u8>, PakError>) -> Option<&'static str> {
        result.as_ref().err().map(|error| match error {
            PakError::BadMagic => "bad_magic",
            PakError::UnsupportedVersion { .. } => "unsupported_version",
            PakError::UnsupportedFlags { .. } => "unsupported_flags",
            PakError::TocDecode(_) => "toc_decode",
            PakError::NotFound(_) => "not_found",
            PakError::HashMismatch { .. } => "hash_mismatch",
            PakError::Io(_) => "io",
            PakError::Corrupt { .. } => "corrupt",
            PakError::TocOutOfBounds { .. } => "toc_bounds",
            PakError::OutOfBounds { .. } => "entry_bounds",
        })
    }

    fn assert_owned_and_mapped_parity(bytes: &[u8], ids: &[AssetId]) {
        let owned = PakReader::from_bytes(bytes.to_vec(), "parity.fpak", bytes.len() as u64);
        let dir = regia_test_utils::unique_temp_dir("pak-reader-parity");
        let path = dir.join("parity.fpak");
        std::fs::write(&path, bytes).expect("write fixture");
        let mapped = PakReader::open(&path);
        assert_eq!(owned.is_ok(), mapped.is_ok(), "open acceptance differs");
        match (owned, mapped) {
            (Ok(owned), Ok(mapped)) => {
                let owned = Arc::new(owned);
                let mapped = Arc::new(mapped);
                for id in ids {
                    let owned_read = owned.read_decompressed(*id);
                    let mapped_read = mapped.read_decompressed(*id);
                    assert_eq!(error_category(&owned_read), error_category(&mapped_read));
                    match (&owned_read, &mapped_read) {
                        (Ok(left), Ok(right)) => assert_eq!(left, right),
                        (Err(left), Err(right)) => {
                            assert_eq!(std::mem::discriminant(left), std::mem::discriminant(right));
                        }
                        _ => unreachable!("read acceptance checked above"),
                    }
                    let owned_shared = owned.read_shared(*id);
                    let mapped_shared = mapped.read_shared(*id);
                    assert_eq!(owned_shared.is_ok(), mapped_shared.is_ok());
                    if let (Err(left), Err(right)) = (&owned_shared, &mapped_shared) {
                        assert_eq!(std::mem::discriminant(left), std::mem::discriminant(right));
                    }
                    match (&owned_shared, &mapped_shared) {
                        (Ok(left), Ok(right)) => {
                            assert_eq!(left, right);
                            if let (Ok(decoded), Ok(mapped_decoded)) = (&owned_read, &mapped_read) {
                                assert_eq!(&**left, decoded);
                                assert_eq!(&**right, mapped_decoded);
                            }
                        }
                        (Err(left), Err(right)) => {
                            assert_eq!(std::mem::discriminant(left), std::mem::discriminant(right));
                        }
                        _ => unreachable!("shared read acceptance checked above"),
                    }
                }
            }
            (Err(left), Err(right)) => {
                assert_eq!(std::mem::discriminant(&left), std::mem::discriminant(&right))
            }
            _ => unreachable!("acceptance checked above"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn owned_and_mapped_entry_paths_have_security_parity() {
        let raw_id = AssetId::new_random();
        let packed_id = AssetId::new_random();
        let raw = randomish_bytes(97);
        let packed = randomish_bytes(PARALLEL_LZ4_MIN_BLOCKS * FPAK_COMPRESS_BLOCK_SIZE + 31);
        let raw_fixture = make_test_pak(vec![(raw_id, raw.clone())], false);
        assert_owned_and_mapped_parity(&raw_fixture, &[raw_id]);
        let packed_fixture = make_test_pak(vec![(packed_id, packed.clone())], true);
        assert_owned_and_mapped_parity(&packed_fixture, &[packed_id]);
        let fixture_reader = PakReader::from_bytes(packed_fixture.clone(), "p.fpak", 0).expect("fixture");
        assert_eq!(
            fixture_reader.read_decompressed(packed_id).expect("decode"),
            packed
        );

        let base = PakReader::from_bytes(packed_fixture.clone(), "p.fpak", 0).expect("fixture");
        let entry = base.entry(packed_id).expect("entry").clone();
        let mut cases = Vec::new();

        let mut toc = base.toc.clone();
        toc.entries[0].offset = u64::MAX;
        cases.push(rebuild_with_toc(&packed_fixture, &toc));
        let mut toc = base.toc.clone();
        toc.entries[0].offset += 1;
        cases.push(rebuild_with_toc(&packed_fixture, &toc));
        let mut toc = base.toc.clone();
        toc.entries[0].block_table_offset += 4;
        cases.push(rebuild_with_toc(&packed_fixture, &toc));
        let mut toc = base.toc.clone();
        toc.entries[0].size -= 1;
        cases.push(rebuild_with_toc(&packed_fixture, &toc));
        let mut toc = base.toc.clone();
        let mut duplicate = toc.entries[0].clone();
        duplicate.offset = entry.offset;
        toc.entries.push(duplicate);
        cases.push(rebuild_with_toc(&packed_fixture, &toc));

        for (toc_offset, toc_len) in [(u64::MAX, 10), (packed_fixture.len() as u64, u64::MAX)] {
            let mut bad_range = packed_fixture.clone();
            bad_range[..60].copy_from_slice(&encode_pak_header(
                FPAK_VERSION,
                flags::COMPRESSED,
                toc_offset,
                toc_len,
                &[0; 32],
            ));
            cases.push(bad_range);
        }

        let mut corrupt_payload = packed_fixture.clone();
        let first_block = entry.offset as usize + PARALLEL_LZ4_MIN_BLOCKS * 4;
        corrupt_payload[first_block..first_block + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        cases.push(corrupt_payload);

        for bytes in cases {
            assert_owned_and_mapped_parity(&bytes, &[packed_id]);
        }

        // One bad compressed entry is isolated from an unrelated raw entry.
        let other_id = AssetId::new_random();
        let other = randomish_bytes(113);
        let mut mixed = make_test_pak(vec![(packed_id, packed), (other_id, other.clone())], true);
        let mixed_reader = PakReader::from_bytes(mixed.clone(), "mixed.fpak", 0).expect("mixed fixture");
        let first = mixed_reader.entry(packed_id).expect("first entry");
        let first_block = first.offset as usize + PARALLEL_LZ4_MIN_BLOCKS * 4;
        mixed[first_block..first_block + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let reader = Arc::new(PakReader::from_bytes(mixed, "mixed.fpak", 0).expect("mixed fixture"));
        assert!(reader.read_decompressed(packed_id).is_err());
        assert_eq!(
            reader.read_decompressed(other_id).expect("unaffected entry"),
            other
        );
    }

    #[test]
    fn lz4_sequential_parallel_threshold_boundary_round_trips() {
        for blocks in [PARALLEL_LZ4_MIN_BLOCKS - 1, PARALLEL_LZ4_MIN_BLOCKS] {
            let id = AssetId::new_random();
            let payload = randomish_bytes(blocks * FPAK_COMPRESS_BLOCK_SIZE - 7);
            let bytes = make_test_pak(vec![(id, payload.clone())], true);
            assert_owned_and_mapped_parity(&bytes, &[id]);
            let reader = PakReader::from_bytes(bytes, "threshold.fpak", 0).expect("open");
            assert_eq!(reader.read_decompressed(id).expect("decode"), payload);
        }
    }

    #[test]
    fn a_bad_block_in_a_parallel_entry_names_the_entry() {
        let id = AssetId::new_random();
        let blocks = PARALLEL_LZ4_MIN_BLOCKS + 3;
        let payload = randomish_bytes((blocks - 1) * FPAK_COMPRESS_BLOCK_SIZE + 5000);
        let mut bytes = make_test_pak(vec![(id, payload)], true);
        let (offset, table) = {
            let reader = PakReader::from_bytes(bytes.clone(), "c.fpak", 0).expect("open");
            let raw = reader.read_raw(id).expect("raw");
            let table: Vec<usize> = (0..blocks)
                .map(|i| u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().expect("4")) as usize)
                .collect();
            (reader.entry(id).expect("entry").offset as usize, table)
        };
        // Block 5 claims 1000 bytes less and the last block 1000 more: the declared total still
        // matches, so the error comes from the (parallel) decode of block 5.
        let field = |i: usize| offset + blocks * 4 + table[i];
        let five = field(5);
        bytes[five..five + 4].copy_from_slice(&((FPAK_COMPRESS_BLOCK_SIZE - 1000) as u32).to_le_bytes());
        let last = field(blocks - 1);
        bytes[last..last + 4].copy_from_slice(&6000u32.to_le_bytes());
        let reader = PakReader::from_bytes(bytes, "c.fpak", 0).expect("open");
        let err = reader.read_decompressed(id).expect_err("corrupt");
        let msg = err.to_string();
        assert!(matches!(err, PakError::Corrupt { .. }), "{msg}");
        assert!(msg.contains(&id.to_string()) && msg.contains("block 5"), "{msg}");
    }

    /// The SIGBUS window of zero-copy loads: a pak truncated in place under a running game. With
    /// the gate closed (the first launch after an update, until `pak_verify` passes), every read
    /// goes through the file handle, so the cut entry is an error and nothing maps it.
    #[cfg(unix)]
    #[test]
    fn a_closed_gate_copies_through_the_file_and_a_truncated_pak_is_an_error_not_a_sigbus() {
        let (a, b, c) = (
            AssetId::new_random(),
            AssetId::new_random(),
            AssetId::new_random(),
        );
        let first = randomish_bytes(5000);
        let second = randomish_bytes(9000);
        let third = randomish_bytes((PARALLEL_LZ4_MIN_BLOCKS + 1) * FPAK_COMPRESS_BLOCK_SIZE);
        let dir = temp_dir("gate");
        let raw_path = dir.join("raw.fpak");
        std::fs::write(
            &raw_path,
            make_test_pak(vec![(a, first.clone()), (b, second.clone())], false),
        )
        .expect("write");
        let packed_path = dir.join("packed.fpak");
        std::fs::write(&packed_path, make_test_pak(vec![(c, third.clone())], true)).expect("write");

        let reader = Arc::new(PakReader::open(&raw_path).expect("open"));
        assert!(reader.zero_copy(), "a pak opened directly serves views");
        reader.set_zero_copy(false);
        let copy = reader.read_shared(a).expect("read");
        assert!(!copy.is_mapped(), "a closed gate copies");
        assert_eq!(copy, first);

        // Cut the pak in the middle of entry `b`, under the open reader.
        let cut = reader.entry(b).expect("b").offset + 10;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&raw_path)
            .and_then(|f| f.set_len(cut))
            .expect("truncate");
        let err = reader.read_shared(b).expect_err("the cut entry is an error");
        assert!(matches!(err, PakError::Corrupt { .. }), "{err}");
        assert!(err.to_string().contains("truncated"), "{err}");
        assert!(reader.read_decompressed(b).is_err());
        assert_eq!(copy, first, "a served copy does not depend on the file");

        // Opening the gate does not bring views back for a file seen changed on disk.
        reader.zero_copy_gate().open();
        let view = reader.read_shared(a).expect("read");
        assert!(!view.is_mapped(), "a changed file is read as copies");
        assert_eq!(view, first);

        // A compressed entry decompresses from the file the same way, and fails cleanly when cut.
        let packed = Arc::new(PakReader::open(&packed_path).expect("open"));
        packed.set_zero_copy(false);
        assert_eq!(packed.read_shared(c).expect("read"), third);
        let cut = packed.entry(c).expect("c").offset + 4096;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&packed_path)
            .and_then(|f| f.set_len(cut))
            .expect("truncate");
        assert!(packed.read_shared(c).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F2 (r2): the gate is open (the normal state after `pak_verify`) and the pak is cut in place
    /// under the running game. The next read notices (`fstat`), and the cut entry is an error,
    /// not a `SIGBUS`; entries before the cut still read, as copies.
    #[cfg(unix)]
    #[test]
    fn a_pak_truncated_while_the_gate_is_open_is_an_error_not_a_sigbus() {
        let (a, b) = (AssetId::new_random(), AssetId::new_random());
        let first = randomish_bytes(5000);
        let second = randomish_bytes(9000);
        let dir = temp_dir("changed");
        let path = dir.join("raw.fpak");
        std::fs::write(&path, make_test_pak(vec![(a, first.clone()), (b, second)], false)).expect("write");
        let reader = Arc::new(PakReader::open(&path).expect("open"));
        assert!(
            reader.read_shared(a).expect("read").is_mapped(),
            "open gate serves views"
        );

        let cut = reader.entry(b).expect("b").offset + 10;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_len(cut))
            .expect("truncate");
        let err = reader.read_shared(b).expect_err("the cut entry is an error");
        assert!(matches!(err, PakError::Corrupt { .. }), "{err}");
        assert!(reader.read_decompressed(b).is_err());
        let before = reader.read_shared(a).expect("entries before the cut still read");
        assert!(!before.is_mapped());
        assert_eq!(before, first);
        assert!(!reader.zero_copy());
        reader.zero_copy_gate().open();
        assert!(!reader.zero_copy(), "the verify gate cannot reopen a changed pak");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression (unsafe audit 2026-10-21): `read_raw` borrowed the mapping unconditionally, so a
    /// pak cut in place under the reader raised `SIGBUS` there (the process dies; this test would
    /// not return). It now goes through the gate and the changed-file check like every other read.
    #[cfg(unix)]
    #[test]
    fn read_raw_follows_the_gate_and_a_truncated_pak_is_an_error_not_a_sigbus() {
        let (a, b) = (AssetId::new_random(), AssetId::new_random());
        let first = randomish_bytes(5000);
        let second = randomish_bytes(9000);
        let dir = regia_test_utils::unique_temp_dir("pak-read-raw-gate");
        let path = dir.join("raw.fpak");
        std::fs::write(
            &path,
            make_test_pak(vec![(a, first.clone()), (b, second.clone())], false),
        )
        .expect("write");
        let reader = PakReader::open(&path).expect("open");

        let view = reader.read_raw(a).expect("raw");
        assert!(
            matches!(view, Cow::Borrowed(_)),
            "an open gate borrows the mapping"
        );
        assert_eq!(&*view, &first[..]);

        reader.set_zero_copy(false);
        let copy = reader.read_raw(b).expect("raw");
        assert!(
            matches!(copy, Cow::Owned(_)),
            "a closed gate copies through the file"
        );
        assert_eq!(&*copy, &second[..]);
        reader.set_zero_copy(true);

        let cut = reader.entry(b).expect("b").offset + 10;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_len(cut))
            .expect("truncate");
        let err = reader.read_raw(b).expect_err("the cut entry is an error");
        assert!(matches!(err, PakError::Corrupt { .. }), "{err}");
        let before = reader.read_raw(a).expect("entries before the cut still read");
        assert!(
            matches!(before, Cow::Owned(_)),
            "a changed file is read as copies"
        );
        assert_eq!(&*before, &first[..]);
        assert_eq!(&*copy, &second[..], "a copy does not depend on the file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Windows half of the audit fix: while a pak is open (and mapped), no other handle may open
    /// it for writing, so nothing rewrites bytes under a live view; reading it stays allowed.
    #[cfg(windows)]
    #[test]
    fn a_mapped_pak_refuses_other_writers() {
        let id = AssetId::new_random();
        let dir = regia_test_utils::unique_temp_dir("pak-share-mode");
        let path = dir.join("shared.fpak");
        std::fs::write(&path, make_test_pak(vec![(id, randomish_bytes(5000))], false)).expect("write");
        let reader = PakReader::open(&path).expect("open");
        assert!(reader.is_mapped());
        assert!(
            std::fs::OpenOptions::new().write(true).open(&path).is_err(),
            "a writer is refused while the pak is mapped"
        );
        assert!(
            std::fs::File::open(&path).is_ok(),
            "readers (pak_verify) still open it"
        );
        drop(reader);
        assert!(
            std::fs::OpenOptions::new().write(true).open(&path).is_ok(),
            "the lock ends with the reader"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Short inputs: every prefix of a valid pak (raw and compressed) either fails to open or
    /// serves every entry through every read path as a value or an error, never a panic.
    #[test]
    fn every_prefix_of_a_pak_is_an_error_or_a_valid_read_never_a_panic() {
        let id = AssetId::new_random();
        let payload = randomish_bytes(700);
        for compressed in [false, true] {
            let bytes = make_test_pak(vec![(id, payload.clone())], compressed);
            let mut opened = 0usize;
            for len in 0..=bytes.len() {
                let prefix = bytes[..len].to_vec();
                let Ok(reader) = PakReader::from_bytes(prefix, "prefix.fpak", len as u64) else {
                    continue;
                };
                opened += 1;
                let reader = Arc::new(reader);
                for entry in &reader.toc.entries {
                    let _ = reader.read_raw(entry.id);
                    let _ = reader.prefetch(entry.id);
                    if let Ok(decoded) = reader.read_decompressed(entry.id) {
                        assert_eq!(decoded, payload, "a prefix that reads reads the whole entry");
                    }
                    if let Ok(shared) = reader.read_shared(entry.id) {
                        assert_eq!(shared, payload);
                    }
                }
            }
            assert_eq!(
                opened, 1,
                "only the whole file has its TOC (compressed: {compressed})"
            );
        }
    }

    /// Unaligned inputs: the header parser reads little-endian fields byte by byte, so a header at
    /// an odd address parses the same as an aligned one, and a buffer one byte short is refused.
    #[test]
    fn header_parses_at_any_alignment_and_refuses_a_short_buffer() {
        let encoded = encode_pak_header(FPAK_VERSION, flags::COMPRESSED, 4096, 77, &[7u8; 32]);
        let aligned = PakHeader::from_bytes(&encoded).expect("aligned");
        for shift in 1..8 {
            let mut buffer = vec![0u8; shift];
            buffer.extend_from_slice(&encoded);
            assert_eq!(
                PakHeader::from_bytes(&buffer[shift..]).expect("unaligned"),
                aligned
            );
            assert!(matches!(
                PakHeader::from_bytes(&buffer[shift..shift + 59]),
                Err(PakError::BadMagic)
            ));
        }
    }

    /// A pak that grew in place keeps its old length in the opened snapshot, so the length
    /// check alone (no mtime change needed) must close the mapping.
    #[cfg(unix)]
    #[test]
    fn a_pak_extended_in_place_stops_serving_views() {
        let a = AssetId::new_random();
        let dir = temp_dir("grown");
        let path = dir.join("raw.fpak");
        std::fs::write(&path, make_test_pak(vec![(a, randomish_bytes(5000))], false)).expect("write");
        let reader = Arc::new(PakReader::open(&path).expect("open"));
        assert!(reader.zero_copy());
        let len = std::fs::metadata(&path).expect("meta").len();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_len(len + 4096))
            .expect("extend");
        assert!(!reader.zero_copy(), "a longer file is a changed file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_file_at_rejects_a_length_past_the_end_before_allocating() {
        let dir = regia_test_utils::unique_temp_dir("pak-read-at-short");
        let path = dir.join("short.bin");
        std::fs::write(&path, [7u8; 10]).expect("write fixture");
        let file = std::fs::File::open(&path).expect("open fixture");
        // A table-of-contents length of 32 TiB would abort the process if it were allocated.
        let err = read_file_at(&file, 4, 1usize << 45).expect_err("past the end");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        let err = read_file_at(&file, u64::MAX, 8).expect_err("offset overflow");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(read_file_at(&file, 4, 6).expect("in range"), vec![7u8; 6]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
