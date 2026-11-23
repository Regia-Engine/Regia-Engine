// SPDX-License-Identifier: MPL-2.0
//! Pak diff: what an update from one build's paks to another's costs a player
//! (`docs/specs/22-tier2.md` § T2.14, ADR-3322).
//!
//! Steam's public SteamPipe documentation describes update patches computed over fixed-size
//! chunks of about 1 MiB: a depot's files are split into chunks, and an update ships the chunks
//! whose bytes changed. Because a Regia pak is one large file, the pak writer's ordering and
//! alignment stability decide how many of those chunks differ between two builds. This module
//! reports, for one old/new pair of paks (or two packaged build directories):
//!
//! - per asset (by [`AssetId`] from the TOC): added, removed, changed (stored-bytes hash), moved
//!   (same stored bytes at a new offset) and unchanged;
//! - per fixed-size file chunk: how many old/new chunks differ;
//! - an estimated patch download under the fixed-chunk model (default chunk 1 MiB, the size
//!   SteamPipe's public documentation describes), labelled an estimate — the real Steam patch
//!   also compresses, deduplicates across depots and reuses existing chunks.
//!
//! The writer stores entries sorted by asset id at 4096-byte alignment
//! (`regia_asset_pipeline::pak_writer`), so a change to one asset moves only the chunks from its
//! own bytes through the TOC; every earlier chunk is byte-identical between builds.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::id::AssetId;
use crate::pak::{PakError, PakReader};

/// The default patch-model chunk size: 1 MiB, the fixed chunk size Steam's public SteamPipe
/// documentation describes for depot updates.
pub const DEFAULT_CHUNK_SIZE: u64 = 1 << 20;

/// Bytes read at a time while hashing a chunk. The chunk is hashed incrementally, so a
/// caller-supplied chunk size is never allocated as one buffer.
const CHUNK_READ_BYTES: usize = 64 * 1024;

/// The public source the patch-size model follows: Steamworks documentation, "SteamPipe"
/// (partner.steamgames.com): depot content is split into fixed-size chunks of about 1 MB and
/// updates ship the chunks that changed. The download number is an estimate, not a measurement
/// of a real Steam build.
pub const ESTIMATE_NOTE: &str = "estimate: fixed-size chunks (~1 MiB) per Steam's public SteamPipe documentation; a real Steam patch also compresses and reuses depot chunks";

/// Which side of the diff failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    /// The first path argument (`old`).
    Old,
    /// The second path argument (`new`).
    New,
}

impl std::fmt::Display for DiffSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Old => "old",
            Self::New => "new",
        })
    }
}

/// What happened to one asset between the two paks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetChange {
    /// Present only in the new pak.
    Added,
    /// Present only in the old pak.
    Removed,
    /// Both paks, but the stored bytes differ.
    Changed,
    /// Same stored bytes at the same offset.
    Unchanged,
    /// Same stored bytes at a different offset (a changed asset shifted the layout).
    Moved,
}

/// The diff of one asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AssetDiff {
    /// Asset id (TOC key on both sides).
    pub id: AssetId,
    /// Project-relative path from the pak manifest, when recorded.
    pub path: Option<String>,
    /// What happened.
    pub change: AssetChange,
    /// Stored byte offset in the old pak (`None` when added).
    pub old_offset: Option<u64>,
    /// Stored byte offset in the new pak (`None` when removed).
    pub new_offset: Option<u64>,
    /// Stored (possibly compressed) byte length in the old pak (`None` when added).
    pub old_size: Option<u64>,
    /// Stored (possibly compressed) byte length in the new pak (`None` when removed).
    pub new_size: Option<u64>,
}

/// Totals over the assets of one pak pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AssetTotals {
    /// Assets only in the new pak.
    pub added: u64,
    /// Assets only in the old pak.
    pub removed: u64,
    /// Assets whose stored bytes differ.
    pub changed: u64,
    /// Assets byte-identical at the same offset.
    pub unchanged: u64,
    /// Assets byte-identical at a new offset.
    pub moved: u64,
}

impl AssetTotals {
    fn note(&mut self, change: AssetChange) {
        match change {
            AssetChange::Added => self.added += 1,
            AssetChange::Removed => self.removed += 1,
            AssetChange::Changed => self.changed += 1,
            AssetChange::Unchanged => self.unchanged += 1,
            AssetChange::Moved => self.moved += 1,
        }
    }
}

/// The fixed-size chunk comparison of one pak pair.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ChunkTotals {
    /// The chunk size the comparison used.
    pub chunk_size: u64,
    /// Chunks byte-identical on both sides (counted on the old file).
    pub unchanged: u64,
    /// Chunks in the shared length whose bytes differ.
    pub changed: u64,
    /// Chunks past the old file's end (the new pak grew).
    pub added: u64,
    /// Chunks past the new file's end (the new pak shrank).
    pub removed: u64,
    /// Total chunks of the old file.
    pub old_chunks: u64,
    /// Total chunks of the new file.
    pub new_chunks: u64,
}

/// The estimated patch download under the fixed-chunk model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PatchEstimate {
    /// Chunk size the model used.
    pub chunk_size: u64,
    /// Bytes a player would download: the new-side bytes of every changed or added chunk.
    pub download_bytes: u64,
    /// New file bytes the old build already has (unchanged chunks; not downloaded).
    pub reused_bytes: u64,
    /// Always [`ESTIMATE_NOTE`]: the model and its public source.
    pub note: &'static str,
}

/// The diff of one pak pair (same relative pak path on both sides).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PakDiff {
    /// The pak's relative path (file name for a single-pak diff, path relative to the side root
    /// for a directory diff).
    pub pak: String,
    /// Old pak length in bytes (`None` when the pak was added).
    pub old_len: Option<u64>,
    /// New pak length in bytes (`None` when the pak was removed).
    pub new_len: Option<u64>,
    /// Per-asset diffs, sorted by asset id.
    pub assets: Vec<AssetDiff>,
    /// Asset totals.
    pub totals: AssetTotals,
    /// Fixed-size chunk comparison.
    pub chunks: ChunkTotals,
    /// Estimated patch download.
    pub estimate: PatchEstimate,
}

/// The diff of two sides (pak files or packaged build directories), every matched pak pair and
/// the sums a report prints first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PakSetDiff {
    /// One entry per pak pair (and per pak that exists on only one side).
    pub paks: Vec<PakDiff>,
    /// Sum of every pair's asset totals.
    pub totals: AssetTotals,
    /// Sum of every pair's chunk counts.
    pub chunks: ChunkTotals,
    /// Sum of every pair's patch estimate.
    pub estimate: PatchEstimate,
    /// The chunk size the comparison used.
    pub chunk_size: u64,
}

/// Errors of [`diff_paks`] and [`diff_sides`].
#[derive(Debug, thiserror::Error)]
pub enum PakDiffError {
    /// A pak could not be opened or parsed (bad magic, truncated TOC, unsupported version).
    #[error("{side} pak {path} does not read: {source}")]
    Open {
        /// Which side failed.
        side: DiffSide,
        /// Path the caller gave.
        path: PathBuf,
        /// The pak reader's error.
        source: PakError,
    },

    /// A side's bytes could not be read for the chunk comparison.
    #[error("{side} {path}: {source}")]
    Io {
        /// Which side failed.
        side: DiffSide,
        /// Path the caller gave.
        path: PathBuf,
        /// The IO error.
        source: io::Error,
    },

    /// A directory side holds no `.fpak` files at all.
    #[error("{side} directory {path} contains no .fpak files")]
    NoPaks {
        /// Which side failed.
        side: DiffSide,
        /// Directory the caller gave.
        path: PathBuf,
    },

    /// The chunk size was zero (or did not parse).
    #[error("chunk size must be at least 1 byte, got {0}")]
    InvalidChunkSize(u64),
}

/// One scanned pak: its TOC entries with a hash of each entry's stored bytes, the manifest's
/// id → path map, and the file length.
struct ScannedPak {
    /// TOC entries, each with the blake3 of its stored bytes.
    entries: Vec<(crate::pak::PakEntry, [u8; 32])>,
    /// Manifest paths by asset id.
    paths: BTreeMap<AssetId, String>,
    /// File length in bytes.
    len: u64,
}

/// Reads a pak and hashes every entry's stored bytes.
///
/// Returns the TOC entries with their stored-byte hashes and the manifest's id → path map.
fn scan_pak(path: &Path, side: DiffSide) -> Result<ScannedPak, PakDiffError> {
    let _span = tracing::info_span!("pak_diff_scan", side = %side, path = %path.display()).entered();
    let reader = PakReader::open(path).map_err(|source| PakDiffError::Open {
        side,
        path: path.to_path_buf(),
        source,
    })?;
    let mut hashes = Vec::with_capacity(reader.toc.entries.len());
    for entry in &reader.toc.entries {
        // `read_raw` is the entry's exact stored bytes (block table plus compressed data, or the
        // raw payload) — what a byte-level chunk diff of the file sees.
        let raw = reader.read_raw(entry.id).map_err(|source| PakDiffError::Open {
            side,
            path: path.to_path_buf(),
            source,
        })?;
        hashes.push((entry.clone(), *blake3::hash(&raw).as_bytes()));
    }
    let paths = reader
        .toc
        .manifest
        .iter()
        .map(|e| (e.id, e.path.clone()))
        .collect();
    Ok(ScannedPak {
        entries: hashes,
        paths,
        len: reader.len_bytes(),
    })
}

/// Classifies every asset of one pak pair.
fn diff_assets(
    old: &[(crate::pak::PakEntry, [u8; 32])],
    new: &[(crate::pak::PakEntry, [u8; 32])],
    old_paths: &BTreeMap<AssetId, String>,
    new_paths: &BTreeMap<AssetId, String>,
) -> (Vec<AssetDiff>, AssetTotals) {
    let old_map: BTreeMap<AssetId, _> = old.iter().map(|(e, h)| (e.id, (e, h))).collect();
    let new_map: BTreeMap<AssetId, _> = new.iter().map(|(e, h)| (e.id, (e, h))).collect();
    let mut diffs = Vec::with_capacity(old.len().max(new.len()));
    let mut totals = AssetTotals::default();
    // Both id sets, merged and deduplicated, in id order.
    let ids: Vec<AssetId> = old_map
        .keys()
        .chain(new_map.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for id in ids {
        let (change, old_e, new_e) = match (old_map.get(&id), new_map.get(&id)) {
            (Some((o, _)), None) => (AssetChange::Removed, Some(*o), None),
            (None, Some((n, _))) => (AssetChange::Added, None, Some(*n)),
            (Some((o, oh)), Some((n, nh))) => {
                if oh == nh {
                    if o.offset == n.offset {
                        (AssetChange::Unchanged, Some(*o), Some(*n))
                    } else {
                        (AssetChange::Moved, Some(*o), Some(*n))
                    }
                } else {
                    (AssetChange::Changed, Some(*o), Some(*n))
                }
            }
            (None, None) => continue,
        };
        totals.note(change);
        diffs.push(AssetDiff {
            id,
            path: new_paths.get(&id).or_else(|| old_paths.get(&id)).cloned(),
            change,
            old_offset: old_e.map(|e| e.offset),
            new_offset: new_e.map(|e| e.offset),
            old_size: old_e.map(|e| e.compressed_size),
            new_size: new_e.map(|e| e.compressed_size),
        });
    }
    (diffs, totals)
}

/// Hashes one file's fixed-size chunks, streaming (the pak is never copied whole, and the chunk
/// itself is never buffered: a chunk size above `isize::MAX` used to panic in `Vec`).
fn hash_chunks(path: &Path, side: DiffSide, chunk_size: u64) -> Result<Vec<[u8; 32]>, PakDiffError> {
    let _span = tracing::info_span!("pak_diff_chunks", side = %side, path = %path.display()).entered();
    let mut file = std::fs::File::open(path).map_err(|source| PakDiffError::Io {
        side,
        path: path.to_path_buf(),
        source,
    })?;
    let buf_len = CHUNK_READ_BYTES
        .min(usize::try_from(chunk_size).unwrap_or(usize::MAX))
        .max(1);
    let mut buf = vec![0u8; buf_len];
    let mut out = Vec::new();
    loop {
        let mut hasher = blake3::Hasher::new();
        let mut got = 0u64;
        while got < chunk_size {
            let want = usize::try_from(chunk_size - got)
                .unwrap_or(usize::MAX)
                .min(buf.len());
            let read = file.read(&mut buf[..want]).map_err(|source| PakDiffError::Io {
                side,
                path: path.to_path_buf(),
                source,
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            got += read as u64;
        }
        if got == 0 {
            break;
        }
        out.push(*hasher.finalize().as_bytes());
        if got < chunk_size {
            break;
        }
    }
    Ok(out)
}

/// Compares the chunk lists of two files and builds the patch estimate.
fn diff_chunks(
    old_path: &Path,
    new_path: &Path,
    chunk_size: u64,
) -> Result<(ChunkTotals, PatchEstimate), PakDiffError> {
    let old = hash_chunks(old_path, DiffSide::Old, chunk_size)?;
    let new = hash_chunks(new_path, DiffSide::New, chunk_size)?;
    let shared = old.len().min(new.len());
    let changed = (0..shared).filter(|&i| old[i] != new[i]).count() as u64;
    let totals = ChunkTotals {
        chunk_size,
        unchanged: shared as u64 - changed,
        changed,
        added: new.len() as u64 - shared as u64,
        removed: old.len() as u64 - shared as u64,
        old_chunks: old.len() as u64,
        new_chunks: new.len() as u64,
    };
    // Download: the new-side bytes of every differing or added chunk (the last chunk may be short).
    let new_len = std::fs::metadata(new_path)
        .map_err(|source| PakDiffError::Io {
            side: DiffSide::New,
            path: new_path.to_path_buf(),
            source,
        })?
        .len();
    let chunk_bytes = |i: u64| new_len.saturating_sub(i * chunk_size).min(chunk_size);
    let download_bytes = (0..shared as u64)
        .filter(|&i| old[i as usize] != new[i as usize])
        .chain(shared as u64..new.len() as u64)
        .map(chunk_bytes)
        .sum();
    let reused_bytes = new_len.saturating_sub(download_bytes);
    Ok((
        totals,
        PatchEstimate {
            chunk_size,
            download_bytes,
            reused_bytes,
            note: ESTIMATE_NOTE,
        },
    ))
}

/// Diffs one pak file against another.
///
/// # Errors
///
/// Either pak does not open (bad magic, truncated file — reported, never a panic), a file cannot
/// be read, or `chunk_size` is 0.
pub fn diff_paks(old: &Path, new: &Path, chunk_size: u64) -> Result<PakDiff, PakDiffError> {
    if chunk_size == 0 {
        return Err(PakDiffError::InvalidChunkSize(chunk_size));
    }
    let _span = tracing::info_span!("pak_diff", old = %old.display(), new = %new.display()).entered();
    let old_scan = scan_pak(old, DiffSide::Old)?;
    let new_scan = scan_pak(new, DiffSide::New)?;
    let (assets, totals) = diff_assets(
        &old_scan.entries,
        &new_scan.entries,
        &old_scan.paths,
        &new_scan.paths,
    );
    let (chunks, estimate) = diff_chunks(old, new, chunk_size)?;
    Ok(PakDiff {
        pak: new
            .file_name()
            .map_or_else(|| "pak".to_string(), |n| n.to_string_lossy().into_owned()),
        old_len: Some(old_scan.len),
        new_len: Some(new_scan.len),
        assets,
        totals,
        chunks,
        estimate,
    })
}

/// The `.fpak` files of one side: a single pak file, or every pak in a directory (recursive,
/// sorted by path relative to the side root).
fn side_paks(path: &Path, side: DiffSide) -> Result<Vec<(String, PathBuf)>, PakDiffError> {
    if path.is_dir() {
        let mut paks = Vec::new();
        collect_paks(path, path, &mut paks).map_err(|source| PakDiffError::Io {
            side,
            path: path.to_path_buf(),
            source,
        })?;
        paks.sort();
        if paks.is_empty() {
            return Err(PakDiffError::NoPaks {
                side,
                path: path.to_path_buf(),
            });
        }
        Ok(paks)
    } else {
        let name = path
            .file_name()
            .map_or_else(|| "pak".to_string(), |n| n.to_string_lossy().into_owned());
        Ok(vec![(name, path.to_path_buf())])
    }
}

/// Appends every `.fpak` under `dir` as (relative path, absolute path) pairs.
///
/// `file_type` does not follow symlinks, so a directory link (including a link to `.`) is not
/// walked. A symlink whose target is a `.fpak` file is included.
fn collect_paks(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_paks(root, &path, out)?;
            continue;
        }
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("fpak")) {
            continue;
        }
        // A symlink is not a file until followed. Follow a link to a file only.
        if file_type.is_symlink() && !path.is_file() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map_or_else(|_| path.clone(), |p| p.to_path_buf());
        out.push((rel.to_string_lossy().into_owned(), path));
    }
    Ok(())
}

/// Diffs two sides: each a `.fpak` file or a directory (a packaged build's `content/`, or the
/// build root — paks are found recursively). Paks pair up by relative path; a pak on only one
/// side reports every asset and chunk as added or removed.
///
/// # Errors
///
/// See [`diff_paks`]; a directory side with no `.fpak` files is [`PakDiffError::NoPaks`].
pub fn diff_sides(old: &Path, new: &Path, chunk_size: u64) -> Result<PakSetDiff, PakDiffError> {
    if chunk_size == 0 {
        return Err(PakDiffError::InvalidChunkSize(chunk_size));
    }
    // Two pak files compare directly, whatever their names (the common case: two builds'
    // base.fpak live in different directories; pairing by name would report one removed and one
    // added instead of a diff).
    if old.is_file() && new.is_file() {
        let pair = diff_paks(old, new, chunk_size)?;
        return Ok(PakSetDiff {
            totals: pair.totals,
            chunks: pair.chunks,
            estimate: pair.estimate,
            chunk_size,
            paks: vec![pair],
        });
    }
    let old_paks = side_paks(old, DiffSide::Old)?;
    let new_paks = side_paks(new, DiffSide::New)?;
    let old_map: BTreeMap<String, PathBuf> = old_paks.into_iter().collect();
    let new_map: BTreeMap<String, PathBuf> = new_paks.into_iter().collect();
    let names: Vec<String> = old_map
        .keys()
        .chain(new_map.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut paks = Vec::with_capacity(names.len());
    let mut totals = AssetTotals::default();
    let mut chunk_totals = ChunkTotals {
        chunk_size,
        ..ChunkTotals::default()
    };
    let mut download_bytes = 0u64;
    let mut reused_bytes = 0u64;
    for name in names {
        let mut pair = match (old_map.get(&name), new_map.get(&name)) {
            (Some(o), Some(n)) => diff_paks(o, n, chunk_size)?,
            (Some(o), None) => single_side_pak(&name, o, false, chunk_size)?,
            (None, Some(n)) => single_side_pak(&name, n, true, chunk_size)?,
            (None, None) => continue,
        };
        // The report's pak key is the pairing name (relative path within a directory side), not
        // the new file's bare name: two pairs in one directory diff must stay distinguishable.
        pair.pak = name.clone();
        totals.added += pair.totals.added;
        totals.removed += pair.totals.removed;
        totals.changed += pair.totals.changed;
        totals.unchanged += pair.totals.unchanged;
        totals.moved += pair.totals.moved;
        chunk_totals.unchanged += pair.chunks.unchanged;
        chunk_totals.changed += pair.chunks.changed;
        chunk_totals.added += pair.chunks.added;
        chunk_totals.removed += pair.chunks.removed;
        chunk_totals.old_chunks += pair.chunks.old_chunks;
        chunk_totals.new_chunks += pair.chunks.new_chunks;
        download_bytes += pair.estimate.download_bytes;
        reused_bytes += pair.estimate.reused_bytes;
        paks.push(pair);
    }
    Ok(PakSetDiff {
        paks,
        totals,
        chunks: chunk_totals,
        estimate: PatchEstimate {
            chunk_size,
            download_bytes,
            reused_bytes,
            note: ESTIMATE_NOTE,
        },
        chunk_size,
    })
}

/// Builds the report for a pak that exists on only one side: every asset and chunk added
/// (new side) or removed (old side).
fn single_side_pak(name: &str, path: &Path, is_new: bool, chunk_size: u64) -> Result<PakDiff, PakDiffError> {
    let side = if is_new { DiffSide::New } else { DiffSide::Old };
    let scanned = scan_pak(path, side)?;
    let mut totals = AssetTotals::default();
    let mut assets = Vec::with_capacity(scanned.entries.len());
    for (entry, _) in &scanned.entries {
        totals.note(if is_new {
            AssetChange::Added
        } else {
            AssetChange::Removed
        });
        assets.push(AssetDiff {
            id: entry.id,
            path: scanned.paths.get(&entry.id).cloned(),
            change: if is_new {
                AssetChange::Added
            } else {
                AssetChange::Removed
            },
            old_offset: (!is_new).then_some(entry.offset),
            new_offset: is_new.then_some(entry.offset),
            old_size: (!is_new).then_some(entry.compressed_size),
            new_size: is_new.then_some(entry.compressed_size),
        });
    }
    let len = scanned.len;
    let chunks = hash_chunks(path, side, chunk_size)?.len() as u64;
    let (chunk_totals, estimate) = if is_new {
        (
            ChunkTotals {
                chunk_size,
                added: chunks,
                new_chunks: chunks,
                ..ChunkTotals::default()
            },
            PatchEstimate {
                chunk_size,
                download_bytes: len,
                reused_bytes: 0,
                note: ESTIMATE_NOTE,
            },
        )
    } else {
        (
            ChunkTotals {
                chunk_size,
                removed: chunks,
                old_chunks: chunks,
                ..ChunkTotals::default()
            },
            PatchEstimate {
                chunk_size,
                download_bytes: 0,
                reused_bytes: 0,
                note: ESTIMATE_NOTE,
            },
        )
    };
    Ok(PakDiff {
        pak: name.to_string(),
        old_len: (!is_new).then_some(len),
        new_len: is_new.then_some(len),
        assets,
        totals,
        chunks: chunk_totals,
        estimate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestEntry;
    use crate::pak::{PakToc, encode_pak_header};

    /// Writes an uncompressed test pak (the writer's layout: id-sorted entries at 4096 alignment).
    fn write_test_pak(path: &Path, payloads: &[(AssetId, Vec<u8>)]) {
        let mut data: Vec<u8> = vec![0u8; 60];
        let mut sorted: Vec<_> = payloads.iter().collect();
        sorted.sort_by_key(|(id, _)| *id);
        let mut toc_entries = Vec::new();
        let mut manifest = Vec::new();
        for (id, payload) in sorted {
            let rem = data.len() % crate::pak::FPAK_BLOB_ALIGNMENT as usize;
            if rem != 0 {
                data.resize(data.len() + (crate::pak::FPAK_BLOB_ALIGNMENT as usize - rem), 0);
            }
            let offset = data.len() as u64;
            data.extend_from_slice(payload);
            toc_entries.push(crate::pak::PakEntry {
                id: *id,
                type_tag: regia_core::id::TypePathHash::of("regia_asset::CookedImage"),
                offset,
                size: payload.len() as u64,
                compressed_size: payload.len() as u64,
                block_table_offset: 0,
            });
            manifest.push(ManifestEntry {
                path: format!("assets/{id}.bin"),
                id: *id,
                type_path: "regia_asset::CookedImage".to_string(),
            });
        }
        let toc_offset = data.len() as u64;
        let toc = PakToc {
            entries: toc_entries,
            manifest,
            dependencies: Vec::new(),
        };
        let toc_bytes = postcard::to_allocvec(&toc).expect("toc encodes in a test");
        data.extend_from_slice(&toc_bytes);
        let content_hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
        data[..60].copy_from_slice(&encode_pak_header(
            crate::pak::FPAK_VERSION,
            0,
            toc_offset,
            toc_bytes.len() as u64,
            &content_hash,
        ));
        std::fs::write(path, &data).expect("test pak writes");
    }

    /// Deterministic ids in creation order (AssetId orders by its u128).
    fn ids(n: usize) -> Vec<AssetId> {
        (0..n)
            .map(|i| AssetId(regia_core::id::Guid(0x10_0000u128 + i as u128)))
            .collect()
    }

    #[test]
    fn identical_paks_give_an_empty_diff() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-identical");
        let ids = ids(3);
        let payloads: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, vec![b'a' + i as u8; 5000]))
            .collect();
        let old = tmp.join("old.fpak");
        let new = tmp.join("new.fpak");
        write_test_pak(&old, &payloads);
        write_test_pak(&new, &payloads);

        let diff = diff_paks(&old, &new, DEFAULT_CHUNK_SIZE).expect("diff");
        assert_eq!(
            diff.totals,
            AssetTotals {
                unchanged: 3,
                ..AssetTotals::default()
            }
        );
        assert_eq!(diff.chunks.changed, 0, "{diff:?}");
        assert_eq!(diff.estimate.download_bytes, 0);
        assert_eq!(
            diff.assets
                .iter()
                .filter(|a| a.change != AssetChange::Unchanged)
                .count(),
            0
        );
    }

    #[test]
    fn one_changed_asset_costs_only_its_chunks_and_the_toc() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-changed");
        let ids = ids(3);
        let mut payloads: Vec<_> = ids.iter().map(|id| (*id, vec![b'x'; 8000])).collect();
        let old = tmp.join("old.fpak");
        let new = tmp.join("new.fpak");
        write_test_pak(&old, &payloads);
        // The last-sorted asset grows: the other two keep their bytes and offsets, and the TOC
        // entry of the edited asset changes (its stored size), so the estimate is exactly the
        // edited asset's chunks plus the TOC (and the header's content hash).
        payloads[2].1 = vec![b'z'; 12000];
        write_test_pak(&new, &payloads);

        let diff = diff_paks(&old, &new, 4096).expect("diff");
        assert_eq!(diff.totals.changed, 1, "{diff:?}");
        assert_eq!(diff.totals.added, 0);
        assert_eq!(diff.totals.removed, 0);
        assert_eq!(diff.totals.unchanged, 2);
        let changed = diff
            .assets
            .iter()
            .find(|a| a.change == AssetChange::Changed)
            .expect("one changed");
        assert_eq!(changed.id, ids[2]);

        // The untouched assets' chunks (two 8000-byte payloads at 4096 alignment: 2 chunks each)
        // must be the only reused ones; everything else (header, edited asset, TOC) differs.
        let file_len = std::fs::metadata(&new).expect("stat").len();
        let chunk_of = |offset: u64, size: u64| -> Vec<u64> {
            let first = offset / 4096;
            let last = (offset + size).saturating_sub(1) / 4096;
            (first..=last).collect()
        };
        let reused: Vec<u64> = diff
            .assets
            .iter()
            .filter(|a| a.change == AssetChange::Unchanged)
            .flat_map(|a| chunk_of(a.new_offset.unwrap_or(0), a.new_size.unwrap_or(0)))
            .collect();
        assert_eq!(reused.len(), 4, "two untouched assets of 2 chunks each: {diff:?}");
        assert_eq!(
            diff.chunks.unchanged,
            reused.len() as u64,
            "exactly the untouched assets' chunks are reused: {diff:?}"
        );
        assert_eq!(
            diff.chunks.changed + diff.chunks.added,
            diff.chunks.new_chunks - reused.len() as u64,
            "every other new chunk differs (header, edited asset, TOC): {diff:?}"
        );
        let expected: u64 = (0..diff.chunks.new_chunks)
            .filter(|i| !reused.contains(i))
            .map(|i| file_len.saturating_sub(i * 4096).min(4096))
            .sum();
        assert_eq!(diff.estimate.download_bytes, expected);
        assert_eq!(diff.estimate.reused_bytes, file_len - expected);
        assert_eq!(diff.estimate.note, ESTIMATE_NOTE);
    }

    #[test]
    fn an_added_asset_reports_added_and_the_new_tail() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-added");
        let ids = ids(4);
        // The added asset sorts last: the existing three keep their offsets.
        let base: Vec<_> = ids[..3].iter().map(|id| (*id, vec![7u8; 3000])).collect();
        let old = tmp.join("old.fpak");
        let new = tmp.join("new.fpak");
        write_test_pak(&old, &base);
        let mut extended = base.clone();
        extended.push((ids[3], vec![9u8; 3000]));
        write_test_pak(&new, &extended);

        let diff = diff_paks(&old, &new, 4096).expect("diff");
        assert_eq!(diff.totals.added, 1, "{diff:?}");
        assert_eq!(diff.totals.unchanged, 3);
        assert_eq!(diff.totals.moved, 0);
        let added = diff
            .assets
            .iter()
            .find(|a| a.change == AssetChange::Added)
            .expect("one added");
        assert_eq!(added.id, ids[3]);
        assert!(diff.chunks.added > 0);
        assert_eq!(
            diff.estimate.download_bytes,
            std::fs::metadata(&new).expect("stat").len() - diff.estimate.reused_bytes
        );
    }

    #[test]
    fn directories_pair_paks_by_relative_path() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-dirs");
        let old_dir = tmp.join("old-build/content");
        let new_dir = tmp.join("new-build/content");
        std::fs::create_dir_all(&old_dir).expect("mkdir");
        std::fs::create_dir_all(&new_dir).expect("mkdir");
        let ids = ids(2);
        let base: Vec<_> = ids.iter().map(|id| (*id, vec![3u8; 2000])).collect();
        write_test_pak(&old_dir.join("base.fpak"), &base);
        write_test_pak(&new_dir.join("base.fpak"), &base);
        // A new chunk pak appears only on the new side.
        write_test_pak(&new_dir.join("chunk_arena.fpak"), &base);

        let diff = diff_sides(
            tmp.join("old-build").as_path(),
            tmp.join("new-build").as_path(),
            4096,
        )
        .expect("diff");
        assert_eq!(diff.paks.len(), 2, "{diff:?}");
        let base = diff
            .paks
            .iter()
            .find(|p| p.pak == "content/base.fpak")
            .expect("base pair");
        assert_eq!(base.totals.unchanged, 2);
        let arena = diff
            .paks
            .iter()
            .find(|p| p.pak == "content/chunk_arena.fpak")
            .expect("arena pair");
        assert_eq!(arena.totals.added, 2);
        assert_eq!(arena.old_len, None);
        assert_eq!(diff.totals.added, 2);
    }

    #[test]
    fn a_truncated_pak_is_an_error_not_a_panic() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-truncated");
        let ids = ids(2);
        let base: Vec<_> = ids.iter().map(|id| (*id, vec![5u8; 2000])).collect();
        let old = tmp.join("old.fpak");
        let new = tmp.join("new.fpak");
        write_test_pak(&old, &base);
        write_test_pak(&new, &base);
        // Cut the new pak in its data region: the TOC range in the header now lies past the end.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&new)
            .expect("open for truncate");
        file.set_len(100).expect("truncate");

        let err = diff_paks(&old, &new, 4096).expect_err("a truncated pak is reported");
        assert!(
            matches!(
                err,
                PakDiffError::Open {
                    side: DiffSide::New,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_zero_chunk_size_is_rejected() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-chunk-zero");
        let err = diff_sides(&tmp, &tmp, 0).expect_err("zero chunk size");
        assert!(matches!(err, PakDiffError::InvalidChunkSize(0)));
    }

    #[test]
    fn audfix_huge_chunk_size_does_not_panic() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-chunk-huge");
        let ids = ids(1);
        let base: Vec<_> = ids.iter().map(|id| (*id, vec![1u8; 32])).collect();
        let old = tmp.join("old.fpak");
        let new = tmp.join("new.fpak");
        write_test_pak(&old, &base);
        write_test_pak(&new, &base);
        // Larger than isize::MAX: allocating a buffer of this size panics before any read.
        let diff = diff_paks(&old, &new, 1_u64 << 63).expect("one chunk, not a panic");
        assert_eq!(diff.chunks.old_chunks, 1);
        assert_eq!(diff.chunks.new_chunks, 1);
        assert_eq!(diff.chunks.unchanged, 1);
        assert_eq!(diff.chunks.changed, 0);
    }

    #[cfg(unix)]
    #[test]
    fn audfix_directory_symlink_cycle_does_not_overflow() {
        let old = regia_test_utils::unique_temp_dir("pak-diff-symlink-old");
        let new = regia_test_utils::unique_temp_dir("pak-diff-symlink-new");
        let store = regia_test_utils::unique_temp_dir("pak-diff-symlink-store");
        let ids = ids(1);
        let payload: Vec<_> = ids.iter().map(|id| (*id, vec![9u8; 64])).collect();
        let real_old = store.join("old.fpak");
        let real_new = store.join("new.fpak");
        write_test_pak(&real_old, &payload);
        write_test_pak(&real_new, &payload);
        std::os::unix::fs::symlink(&real_old, old.join("base.fpak")).expect("old link");
        std::os::unix::fs::symlink(&real_new, new.join("base.fpak")).expect("new link");
        std::os::unix::fs::symlink(".", old.join("loop")).expect("old cycle");
        std::os::unix::fs::symlink(".", new.join("loop")).expect("new cycle");
        let diff = diff_sides(&old, &new, 4096).expect("a directory cycle is not walked");
        assert_eq!(diff.paks.len(), 1, "{diff:?}");
        assert_eq!(diff.paks[0].pak, "base.fpak");
        assert_eq!(diff.totals.unchanged, 1);
    }

    #[test]
    fn a_directory_without_paks_is_an_error() {
        let tmp = regia_test_utils::unique_temp_dir("pak-diff-no-paks");
        let other = regia_test_utils::unique_temp_dir("pak-diff-no-paks-other");
        let ids = ids(1);
        let base: Vec<_> = ids.iter().map(|id| (*id, vec![1u8; 100])).collect();
        write_test_pak(&other.join("base.fpak"), &base);
        let err = diff_sides(&tmp, &other, 4096).expect_err("no paks on the old side");
        assert!(
            matches!(
                err,
                PakDiffError::NoPaks {
                    side: DiffSide::Old,
                    ..
                }
            ),
            "{err}"
        );
    }
}
