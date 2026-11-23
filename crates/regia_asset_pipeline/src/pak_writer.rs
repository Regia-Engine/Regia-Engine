// SPDX-License-Identifier: MPL-2.0
//! Pak file writer: packs cooked assets into `.fpak` archives
//! (`docs/specs/12-assets.md` § 5).
//!
//! Used by `regia cook` (and later the editor) to produce shippable pak files
//! from the cooked asset cache.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regia_asset::{
    AssetId, AssetManifest, FPAK_BLOB_ALIGNMENT, FPAK_COMPRESS_BLOCK_SIZE, FPAK_MAX_BLOCK_RATIO,
    FPAK_VERSION, PakEntry, PakToc, encode_pak_header, flags as pak_flags,
};
use regia_core::id::TypePathHash;
use tracing::instrument;

/// Options controlling pak creation.
#[derive(Clone, Debug)]
pub struct PakWriteOptions {
    /// Whether to lz4 block-compress asset blobs (default true on shipping).
    pub compress: bool,
    /// The chunk name (used in the output file name `chunk_<name>.fpak`).
    pub chunk_name: String,
    /// Paths of paks this one depends on, stored in the TOC. Mounting orders by them: every
    /// listed dependency mounts before this pak (matched by case-folded file name or stem), so a
    /// patch pak no longer needs a file name that sorts after its base. spec 12 § 5.
    pub dependencies: Vec<String>,
    /// With `compress`: the smallest fraction of a blob's bytes lz4 must save for the blob to be
    /// stored compressed; a blob that saves less (`1 - stored / raw < min_ratio`, with the block
    /// table counted as stored bytes) is stored raw, so the reader serves it as a view of the
    /// mapping with no decompress (ADR-7971). `0.0` (default) keeps every blob lz4 shrinks at
    /// all; `regia cook --ship` uses [`SHIP_MIN_RATIO`]. Values are clamped to `0.0..=1.0`
    /// (NaN reads as `0.0`).
    pub min_ratio: f32,
}

impl Default for PakWriteOptions {
    fn default() -> Self {
        Self {
            compress: true,
            chunk_name: "base".to_string(),
            dependencies: Vec::new(),
            min_ratio: 0.0,
        }
    }
}

/// The [`PakWriteOptions::min_ratio`] `regia cook --ship` writes with: lz4 must save at least
/// 15 % of a blob, so block-compressed textures (BC7 typically saves 5-10 %) are stored raw and
/// load as mapping views, while meshes, graphs and metadata stay lz4 (ADR-7971).
pub const SHIP_MIN_RATIO: f32 = 0.15;

/// Whether an entry whose lz4 form (block table plus blocks) is `stored` bytes is kept compressed
/// for a `raw`-byte payload: it must be smaller, and save at least `min_ratio` of `raw`.
fn keeps_compressed(stored: usize, raw: usize, min_ratio: f32) -> bool {
    if stored >= raw {
        return false;
    }
    // Exact integer compare at parts-per-million precision, so 15 % of 100 bytes is 15 bytes
    // (`0.15_f32` is slightly above 0.15, and float products would round the threshold up).
    let ratio = if min_ratio.is_nan() {
        0.0
    } else {
        f64::from(min_ratio.clamp(0.0, 1.0))
    };
    let ppm = (ratio * 1_000_000.0).round() as u128;
    (raw - stored) as u128 * 1_000_000 >= ppm * raw as u128
}

/// Environment variable: `1`/`on`/`true` makes [`pack_cooked_directory`] store the meshes of desktop
/// paks GPU-ready (opt-in: hosts that decode mesh entries themselves keep working by default).
pub const PAK_GPU_MESHES_ENV: &str = "REGIA_PAK_GPU_MESHES";

/// Whether [`pack_cooked_directory`] stores the meshes of the paks it builds from `cooked_dir`
/// (`.regia/cooked/<platform>`) in the GPU-ready layout (`regia_asset::gpu_mesh`): every
/// platform but Android, and only when [`PAK_GPU_MESHES_ENV`] asks for it.
#[must_use]
pub fn gpu_meshes_for(cooked_dir: &Path) -> bool {
    gpu_meshes_with(
        cooked_dir,
        regia_core::brand::env_var(PAK_GPU_MESHES_ENV).ok().as_deref(),
    )
}

/// [`gpu_meshes_for`] with the environment value passed in (testable without touching the process).
fn gpu_meshes_with(cooked_dir: &Path, env: Option<&str>) -> bool {
    let android = cooked_dir
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("android"));
    let on = env.is_some_and(|v| matches!(v.trim(), "1" | "on" | "true"));
    !android && on
}

/// Write a single pak file from a set of cooked assets.
///
/// `cooked_dir` is `.regia/cooked/<platform>/` containing individual
/// `{id}.fckd` files and `asset_manifest.bin`.  `entries` maps each asset
/// id to its type tag (from the manifest). Meshes are stored as cooked (compact);
/// [`pack_cooked_directory`] stores them GPU-ready for desktop platforms when
/// [`PAK_GPU_MESHES_ENV`] is set.
#[instrument(skip_all, fields(out = %out_path.display(), count = entries.len()))]
pub fn write_pak(
    out_path: &Path,
    cooked_dir: &Path,
    entries: &[(AssetId, TypePathHash)],
    manifest: &AssetManifest,
    options: &PakWriteOptions,
) -> Result<(), PakWriteError> {
    write_pak_with(
        out_path,
        entries,
        manifest,
        options,
        false,
        read_cooked_from(cooked_dir),
    )
}

/// Reads entries from a cooked cache directory.
pub(crate) fn read_cooked_from(
    cooked_dir: &Path,
) -> impl FnMut(AssetId) -> Result<Vec<u8>, PakWriteError> + '_ {
    move |id| {
        let cooked_path = cooked_dir.join(cooked_file_name(id));
        std::fs::read(&cooked_path).map_err(|e| PakWriteError::MissingCooked {
            id,
            path: cooked_path,
            source: e,
        })
    }
}

/// The pak-level migrator for paks built before the GPU-ready mesh layout (ADR-285): rewrites
/// `src` as `dst` with every compact mesh converted, everything else (ids, manifest,
/// dependencies, the other entries' bytes) unchanged. Entries are lz4-compressed when `compress`
/// (as `regia package` does). Returns how many meshes were converted.
///
/// # Errors
///
/// `src` does not open or an entry does not read; `dst` cannot be written.
pub fn migrate_pak_meshes(src: &Path, dst: &Path, compress: bool) -> Result<usize, PakWriteError> {
    let reader = regia_asset::PakReader::open(src)
        .map_err(|e| std::io::Error::other(format!("{}: {e}", src.display())))?;
    let entries: Vec<(AssetId, TypePathHash)> =
        reader.toc.entries.iter().map(|e| (e.id, e.type_tag)).collect();
    let compact = entries
        .iter()
        .filter(|(_, tag)| *tag == regia_asset::AssetTypeTag::mesh().0)
        .count();
    let manifest = AssetManifest {
        entries: reader.toc.manifest.clone(),
    };
    let options = PakWriteOptions {
        compress,
        chunk_name: dst
            .file_stem()
            .map_or_else(|| "base".to_string(), |s| s.to_string_lossy().into_owned()),
        dependencies: reader.toc.dependencies.clone(),
        ..PakWriteOptions::default()
    };
    write_pak_with(dst, &entries, &manifest, &options, true, |id| {
        reader
            .read_decompressed(id)
            .map_err(|e| PakWriteError::Io(std::io::Error::other(format!("{}: {e}", src.display()))))
    })?;
    let written = regia_asset::PakReader::open(dst)
        .map_err(|e| std::io::Error::other(format!("{}: {e}", dst.display())))?;
    let gpu = regia_asset::AssetTypeTag::gpu_mesh().0;
    let converted = written.toc.entries.iter().filter(|e| e.type_tag == gpu).count();
    if converted < compact {
        tracing::warn!(target: "regia_asset_pipeline", "{} of {compact} meshes stayed compact", compact - converted);
    }
    Ok(converted)
}

/// The cooked file of an entry as it goes into the pak: a compact mesh becomes GPU-ready when
/// `gpu_meshes` (falling back to the compact form, with a warning, if it does not convert).
fn pak_payload(
    payload: Vec<u8>,
    type_tag: TypePathHash,
    gpu_meshes: bool,
    id: AssetId,
) -> (Vec<u8>, TypePathHash) {
    if !gpu_meshes || type_tag != regia_asset::AssetTypeTag::mesh().0 {
        return (payload, type_tag);
    }
    match gpu_mesh_file_with_submeshes(&payload) {
        Ok(Some(gpu)) => (gpu, regia_asset::AssetTypeTag::gpu_mesh().0),
        Ok(None) => (payload, type_tag),
        Err(e) => {
            tracing::warn!(target: "regia_asset_pipeline", %id, "mesh kept compact in the pak (GPU-ready conversion failed: {e})");
            (payload, type_tag)
        }
    }
}

/// The GPU-ready cooked file for a compact mesh file, keeping the submesh ranges (layout v2,
/// ADR-2788) and the skin and morph block (layout v3, ADR-10080):
/// [`regia_render::cooked_model::gpu_mesh_file_from_cooked`] builds the vertices, LODs
/// and indices, then the compact mesh's submesh table is carried over in index units, so a pak mesh
/// splits by material exactly as the loose file does. `None` for a payload that is not a compact
/// mesh, or one that must stay compact.
fn gpu_mesh_file_with_submeshes(file: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let Some(gpu) = regia_render::cooked_model::gpu_mesh_file_from_cooked(file).map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    let (header, payload) = regia_asset::decode_cooked_file(file).map_err(|e| e.to_string())?;
    let mesh = regia_asset::CookedMesh::from_postcard(payload).map_err(|e| e.to_string())?;
    if mesh.submeshes.is_empty() {
        return Ok(Some(gpu));
    }
    let width: u32 = match mesh.index_width {
        regia_asset::CookedIndexWidth::U16 => 2,
        regia_asset::CookedIndexWidth::U32 => 4,
    };
    let mut submeshes = Vec::with_capacity(mesh.submeshes.len());
    for sub in &mesh.submeshes {
        if !sub.index_byte_offset.is_multiple_of(width) || !sub.index_byte_length.is_multiple_of(width) {
            return Err(format!(
                "submesh range {}+{} is not aligned to {width}-byte indices",
                sub.index_byte_offset, sub.index_byte_length
            ));
        }
        submeshes.push(regia_asset::GpuMeshSubmesh {
            first_index: sub.index_byte_offset / width,
            index_count: sub.index_byte_length / width,
            material_slot: sub.material_slot,
        });
    }
    let (_, gpu_payload) = regia_asset::decode_cooked_file(&gpu).map_err(|e| e.to_string())?;
    let view = regia_asset::GpuMeshView::parse(gpu_payload).map_err(|e| e.to_string())?;
    let indices: Vec<u32> = view
        .indices
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    // The trailing block (skin and morph targets, GPU mesh v3, ADR-10080) is carried over too.
    regia_asset::encode_gpu_mesh_file_parts(
        header.asset_id,
        header.importer_version,
        header.dependencies,
        &regia_asset::GpuMeshParts {
            info: &view.info,
            submeshes: &submeshes,
            vertices: view.vertices,
            indices: &indices,
            extras: view.extras.as_ref(),
        },
    )
    .map(Some)
    .map_err(|e| e.to_string())
}

pub(crate) fn write_pak_with(
    out_path: &Path,
    entries: &[(AssetId, TypePathHash)],
    manifest: &AssetManifest,
    options: &PakWriteOptions,
    gpu_meshes: bool,
    mut read: impl FnMut(AssetId) -> Result<Vec<u8>, PakWriteError>,
) -> Result<(), PakWriteError> {
    let mut data: Vec<u8> = Vec::new();
    let mut toc_entries: Vec<PakEntry> = Vec::new();

    // Reserve space for the 60-byte header.
    data.extend_from_slice(&[0u8; 60]);

    // Sort entries by id for deterministic output.
    let mut sorted: Vec<_> = entries.iter().collect();
    sorted.sort_by_key(|(id, _)| *id);

    for (id, type_tag) in &sorted {
        let payload = read(*id)?;
        let (payload, type_tag) = pak_payload(payload, *type_tag, gpu_meshes, *id);
        let type_tag = &type_tag;

        // Align to 4096.
        let rem = data.len() % FPAK_BLOB_ALIGNMENT as usize;
        if rem != 0 {
            data.resize(data.len() + (FPAK_BLOB_ALIGNMENT as usize - rem), 0);
        }

        let offset = data.len() as u64;
        let size = payload.len() as u64;

        if options.compress && !payload.is_empty() {
            // Compress in blocks.
            let mut compressed_blocks = Vec::new();
            let mut block_lengths = Vec::new();

            for chunk in payload.chunks(FPAK_COMPRESS_BLOCK_SIZE) {
                let compressed = lz4_flex::compress_prepend_size(chunk);
                block_lengths.push(compressed.len());
                compressed_blocks.extend_from_slice(&compressed);
            }
            // The block table holds `u32` starts: an entry whose blocks start past 4 GiB is stored
            // raw (audit v0.2.12: the running offset wrapped and the pak read back as corrupt).
            let block_offsets = block_starts(&block_lengths);

            // A highly-compressible payload (flat terrain, a solid-colour texture, a run of zeros)
            // compresses far past the reader's decompression-bomb cap: the reader rejects an entry
            // whose summed prepended length exceeds [`FPAK_MAX_BLOCK_RATIO`] × its stored compressed
            // bytes (`regia_asset::pak::read_decompressed`). Writing such an entry compressed would
            // produce a pak the reader refuses to load, so store it uncompressed instead (the reader
            // treats `size == compressed_size` as "stored raw"). Real lz4 game data sits below ~10:1,
            // so only the flat/zero/solid cases cross this threshold and pay the storage cost.
            //
            // Compression must also save space: an entry whose block table plus blocks is not
            // smaller than the payload is stored raw. That also keeps a compressed entry from
            // storing exactly `size` bytes, which readers before 2026-10-08 took for "stored raw"
            // (they returned the block table instead of the asset: the gate's `Cooked(BadMagic)`).
            // With `min_ratio`, it must save at least that fraction: a BC7 texture lz4 shrinks by
            // 5-10 % is cheaper to load raw (a mapping view) than to decompress (ADR-7971).
            let block_offsets = block_offsets.filter(|offsets| {
                let stored_len = offsets.len() * 4 + compressed_blocks.len();
                keeps_compressed(stored_len, size as usize, options.min_ratio)
                    && size as usize <= compressed_blocks.len().saturating_mul(FPAK_MAX_BLOCK_RATIO)
            });

            if let Some(block_offsets) = block_offsets {
                // Write block offset table immediately before the compressed data.
                let block_table_offset = data.len() as u64;
                for off in &block_offsets {
                    data.extend_from_slice(&off.to_le_bytes());
                }
                let compressed_data_start = data.len();
                data.extend_from_slice(&compressed_blocks);
                let compressed_size =
                    (compressed_data_start - offset as usize + compressed_blocks.len()) as u64;

                toc_entries.push(PakEntry {
                    id: *id,
                    type_tag: *type_tag,
                    offset,
                    size,
                    compressed_size,
                    block_table_offset,
                });
            } else {
                data.extend_from_slice(&payload);
                toc_entries.push(PakEntry {
                    id: *id,
                    type_tag: *type_tag,
                    offset,
                    size,
                    compressed_size: size,
                    block_table_offset: 0,
                });
            }
        } else {
            data.extend_from_slice(&payload);
            toc_entries.push(PakEntry {
                id: *id,
                type_tag: *type_tag,
                offset,
                size,
                compressed_size: size,
                block_table_offset: 0,
            });
        }
    }

    // Build TOC.
    let toc_offset = data.len() as u64;
    let toc = PakToc {
        entries: toc_entries,
        manifest: manifest.entries.clone(),
        dependencies: options.dependencies.clone(),
    };
    let toc_bytes = postcard::to_allocvec(&toc).map_err(PakWriteError::TocEncode)?;
    let toc_len = toc_bytes.len() as u64;
    data.extend_from_slice(&toc_bytes);

    // Content hash of everything before TOC.
    let content_hash = *blake3::hash(&data[..toc_offset as usize]).as_bytes();
    let flags = if options.compress {
        pak_flags::COMPRESSED
    } else {
        0
    };
    let header = encode_pak_header(FPAK_VERSION, flags, toc_offset, toc_len, &content_hash);
    data[..60].copy_from_slice(&header);

    write_replacing(out_path, &data)?;
    Ok(())
}

/// The start of each compressed block in an entry's `u32` block table, from the blocks' lengths;
/// `None` when a block would start past `u32::MAX` (the entry is then stored raw).
fn block_starts(lengths: &[usize]) -> Option<Vec<u32>> {
    let mut starts = Vec::with_capacity(lengths.len());
    let mut at: u64 = 0;
    for len in lengths {
        starts.push(u32::try_from(at).ok()?);
        at = at.saturating_add(*len as u64);
    }
    Some(starts)
}

/// Write `bytes` to `path` atomically: a temp file in the same directory, synced, then renamed over
/// `path`. A reader never maps a half-written pak, and a pak that is re-cooked while a reader has
/// it mapped keeps the reader's mapping on the old inode — rewriting in place truncated the mapped
/// file under the reader (`SIGBUS`) or let it map a file whose TOC was not written yet.
fn write_replacing(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .map_or_else(|| "pak".into(), |n| n.to_string_lossy());
    let tmp = path.with_file_name(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // Close the write handle before the rename: on Windows a reader opens a pak without
        // `FILE_SHARE_WRITE` (`regia_asset::pak`), so a pak renamed into place while this handle
        // is still open refuses every reader with a sharing violation (os error 32).
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// Write all assets from a cooked directory into one or more pak files.
///
/// Reads `asset_manifest.bin` from `cooked_dir` and produces `.fpak` files
/// in `out_dir`, one per chunk.  A single default chunk `base.fpak` is
/// written when no chunking is configured.
#[instrument(skip_all, fields(cooked = %cooked_dir.display(), out = %out_dir.display()))]
pub fn pack_cooked_directory(
    cooked_dir: &Path,
    out_dir: &Path,
    chunks: &BTreeMap<String, Vec<String>>,
    compress: bool,
) -> Result<Vec<PathBuf>, PakWriteError> {
    pack_cooked_directory_with(cooked_dir, out_dir, chunks, compress, gpu_meshes_for(cooked_dir))
}

/// [`pack_cooked_directory`] with the GPU-ready mesh choice made by the caller instead of
/// [`gpu_meshes_for`]. Cooked assets that land in no pak (see [`CookedPack::skipped`]) are counted
/// in a warning; [`pack_cooked_directory_report`] returns them.
#[instrument(skip_all, fields(cooked = %cooked_dir.display(), out = %out_dir.display()))]
pub fn pack_cooked_directory_with(
    cooked_dir: &Path,
    out_dir: &Path,
    chunks: &BTreeMap<String, Vec<String>>,
    compress: bool,
    gpu_meshes: bool,
) -> Result<Vec<PathBuf>, PakWriteError> {
    let pack = pack_cooked_directory_report(cooked_dir, out_dir, chunks, compress, gpu_meshes)?;
    if let Some(first) = pack.skipped.first() {
        tracing::warn!(
            target: "regia_asset_pipeline",
            count = pack.skipped.len(),
            first = %first,
            "cooked assets are in no pak: no chunk pattern selects them, or only the `{UNMATCHED_CHUNK}` placeholder does"
        );
    }
    Ok(pack.paks)
}

/// The chunk name [`pack_cooked_directory`] never writes a pak for: a classifier's placeholder
/// for assets no rule selects. Its assets are reported in [`CookedPack::skipped`].
pub const UNMATCHED_CHUNK: &str = "__unmatched";

/// What [`pack_cooked_directory_report`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CookedPack {
    /// The paks written, in chunk order (`base.fpak` alone without chunks).
    pub paks: Vec<PathBuf>,
    /// The manifest paths of cooked assets that are in no pak, sorted: with chunks, assets no
    /// chunk's patterns select and assets only the [`UNMATCHED_CHUNK`] placeholder selects. Empty
    /// without chunks (everything goes into `base.fpak`). The rule-based chunk writer
    /// (`chunks::write_chunks`) puts such assets in `base` instead (spec 40 § 2.2 step 2).
    pub skipped: Vec<String>,
}

/// [`pack_cooked_directory_with`], returning the cooked assets the chunks leave out of every pak
/// with the paks (deep-zai-assets A8: they used to be dropped without a word).
#[instrument(skip_all, fields(cooked = %cooked_dir.display(), out = %out_dir.display()))]
pub fn pack_cooked_directory_report(
    cooked_dir: &Path,
    out_dir: &Path,
    chunks: &BTreeMap<String, Vec<String>>,
    compress: bool,
    gpu_meshes: bool,
) -> Result<CookedPack, PakWriteError> {
    let manifest = AssetManifest::read_from_cache(cooked_dir)
        .map_err(|e| PakWriteError::Manifest(e.to_string()))?
        .unwrap_or_default();

    // Gather cooked files that exist on disk.
    let mut asset_map: BTreeMap<String, (AssetId, TypePathHash)> = BTreeMap::new();
    for entry in &manifest.entries {
        let cooked_name = cooked_file_name(entry.id);
        if cooked_dir.join(&cooked_name).is_file() {
            asset_map.insert(entry.path.clone(), (entry.id, TypePathHash::of(&entry.type_path)));
        }
    }

    if chunks.is_empty() || chunks.iter().all(|(_, paths)| paths.is_empty()) {
        // Single pak for all assets.
        let entries: Vec<_> = asset_map.values().cloned().collect();
        let out_path = out_dir.join("base.fpak");
        write_pak_with(
            &out_path,
            &entries,
            &manifest,
            &PakWriteOptions {
                compress,
                chunk_name: "base".to_string(),
                dependencies: Vec::new(),
                ..PakWriteOptions::default()
            },
            gpu_meshes,
            read_cooked_from(cooked_dir),
        )?;
        write_pak_registry_sibling(cooked_dir, &out_path, &manifest);
        return Ok(CookedPack {
            paks: vec![out_path],
            skipped: Vec::new(),
        });
    }

    // Per-chunk paks.
    let mut paks = Vec::new();
    let mut packed: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for (chunk_name, paths) in chunks {
        if chunk_name == UNMATCHED_CHUNK {
            continue;
        }
        let entries: Vec<_> = asset_map
            .iter()
            .filter(|(p, _)| paths.iter().any(|glob| simple_glob_match(p, glob)))
            .map(|(p, v)| {
                packed.insert(p.as_str());
                *v
            })
            .collect();
        if entries.is_empty() {
            continue;
        }
        // Build a sub-manifest for this chunk.
        let chunk_manifest_entries: Vec<_> = manifest
            .entries
            .iter()
            .filter(|e| paths.iter().any(|g| simple_glob_match(&e.path, g)))
            .cloned()
            .collect();
        let chunk_manifest = AssetManifest {
            entries: chunk_manifest_entries,
        };

        let out_path = out_dir.join(format!("chunk_{chunk_name}.fpak"));
        write_pak_with(
            &out_path,
            &entries,
            &chunk_manifest,
            &PakWriteOptions {
                compress,
                chunk_name: chunk_name.clone(),
                dependencies: Vec::new(),
                ..PakWriteOptions::default()
            },
            gpu_meshes,
            read_cooked_from(cooked_dir),
        )?;
        write_pak_registry_sibling(cooked_dir, &out_path, &chunk_manifest);
        paks.push(out_path);
    }
    let skipped = asset_map
        .keys()
        .filter(|p| !packed.contains(p.as_str()))
        .cloned()
        .collect();
    Ok(CookedPack { paks, skipped })
}

/// Chunk pattern match: `**` any run (separators included), `*` any run within a segment, `?`
/// one character (`lod_classes::glob_match`); a pattern without wildcards is an exact path. A
/// trailing `/**` matches the directory itself and its contents, on a path boundary (`props/**`
/// does not match `props_hd/...`).
fn simple_glob_match(path: &str, glob: &str) -> bool {
    glob.strip_suffix("/**") == Some(path) || crate::lod_classes::glob_match(glob, path)
}

/// Writes the asset registry of a pak's entries next to it (`<pak stem>.registry.bin`, spec
/// content-data § 2.1). Derived data: a failure is logged and the pak is still good.
pub(crate) fn write_pak_registry_sibling(cooked_dir: &Path, pak: &Path, manifest: &AssetManifest) {
    let root = crate::registry_build::project_root_of_cache(cooked_dir);
    if let Err(e) = crate::registry_build::write_pak_registry(root, cooked_dir, pak, manifest) {
        tracing::warn!(target: "regia_asset_pipeline", pak = %pak.display(), "asset registry not written: {e}");
    }
}

/// Cooked file name for an asset id — delegates to the asset crate's naming convention.
fn cooked_file_name(id: AssetId) -> String {
    regia_asset::cooked_file_name(id)
}

/// Errors from pak writing.
#[derive(Debug, thiserror::Error)]
pub enum PakWriteError {
    /// A cooked asset file is missing from the cache.
    #[error("cooked file for {id} not found at {path}: {source}")]
    MissingCooked {
        /// Asset id.
        id: AssetId,
        /// Expected path.
        path: PathBuf,
        /// IO error.
        source: std::io::Error,
    },

    /// Postcard encoding of the TOC failed.
    #[error("TOC encode error: {0}")]
    TocEncode(postcard::Error),

    /// Manifest read error.
    #[error("manifest error: {0}")]
    Manifest(String),

    /// IO error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::ManifestEntry;
    use regia_asset::cooked_file_name as asset_cooked_file_name;

    /// Audit v0.2.12 (content L8): the `u32` running offset wrapped past 4 GiB of compressed blocks.
    #[test]
    fn block_starts_past_four_gib_store_the_entry_raw() {
        assert_eq!(block_starts(&[10, 20, 5]), Some(vec![0, 10, 30]));
        assert_eq!(block_starts(&[]), Some(vec![]));
        let block = 1usize << 30;
        // Four 1 GiB blocks: the fourth starts at 3 GiB, still a u32.
        assert!(block_starts(&[block, block, block, block]).is_some());
        // A fifth starts at exactly 4 GiB: past the table.
        assert_eq!(block_starts(&[block, block, block, block, 1]), None);
    }

    fn make_cooked_dir(prefix: &str) -> (PathBuf, PathBuf, Vec<(AssetId, TypePathHash)>) {
        let pid = std::process::id();
        let tmp = std::env::temp_dir().join(format!("regia_pak_test_{pid}_{prefix}"));
        let _ = std::fs::remove_dir_all(&tmp);
        let cooked = tmp.join("cooked");
        std::fs::create_dir_all(&cooked).expect("mkdir");

        let mut entries = Vec::new();
        let mut manifest_entries = Vec::new();
        for i in 0..5 {
            let id = AssetId::new_random();
            let tag = TypePathHash::of("regia_asset::CpuMesh");
            let payload = format!("asset data {i}").into_bytes();
            let name = asset_cooked_file_name(id);
            std::fs::write(cooked.join(&name), &payload).expect("write");
            entries.push((id, tag));
            manifest_entries.push(ManifestEntry {
                path: format!("assets/mesh_{i}.glb"),
                id,
                type_path: "regia_asset::CpuMesh".to_string(),
            });
        }
        let manifest = AssetManifest {
            entries: manifest_entries,
        };
        manifest.write_to_cache(&cooked).expect("write manifest");
        (tmp, cooked, entries)
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A cooked directory containing exactly one asset with the given payload.
    fn make_single_cooked_asset(prefix: &str, payload: &[u8]) -> (PathBuf, PathBuf, AssetId) {
        let pid = std::process::id();
        let tmp = std::env::temp_dir().join(format!("regia_pak_test_{pid}_{prefix}"));
        let _ = std::fs::remove_dir_all(&tmp);
        let cooked = tmp.join("cooked");
        std::fs::create_dir_all(&cooked).expect("mkdir");

        let id = AssetId::new_random();
        let name = asset_cooked_file_name(id);
        std::fs::write(cooked.join(&name), payload).expect("write");
        let manifest = AssetManifest {
            entries: vec![ManifestEntry {
                path: "assets/highly_compressible.bin".to_string(),
                id,
                type_path: "regia_asset::CookedTexture".to_string(),
            }],
        };
        manifest.write_to_cache(&cooked).expect("write manifest");
        (tmp, cooked, id)
    }

    #[test]
    fn write_and_read_pak() {
        let (tmp, cooked_dir, entries) = make_cooked_dir("wr");
        let out = tmp.join("out");
        std::fs::create_dir_all(&out).expect("mkdir");

        let manifest = AssetManifest::read_from_cache(&cooked_dir)
            .expect("read")
            .expect("exists");

        write_pak(
            &out.join("base.fpak"),
            &cooked_dir,
            &entries,
            &manifest,
            &PakWriteOptions::default(),
        )
        .expect("write pak");

        let reader = regia_asset::PakReader::open(&out.join("base.fpak")).expect("open");
        assert_eq!(reader.toc.entries.len(), 5);
        // The runtime integrity check accepts what the writer hashed (no false alarms).
        assert_eq!(
            regia_asset::verify_pak_file(&out.join("base.fpak")).expect("verify"),
            regia_asset::PakIntegrity::Ok
        );
        for (id, _) in &entries {
            let payload = reader.read_decompressed(*id).expect("read");
            let expected = format!(
                "asset data {}",
                entries
                    .iter()
                    .position(|(i, _)| i == id)
                    .expect("entry must exist")
            );
            assert_eq!(payload, expected.as_bytes());
        }
        cleanup(&tmp);
    }

    /// A compact cooked mesh file for `id` (a quad with two LODs), as the cook writes it.
    fn compact_mesh_file(id: AssetId) -> Vec<u8> {
        let cpu = regia_asset::CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.3]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            tangents: Vec::new(),
            indices: vec![0, 1, 2, 0, 2, 3],
            ..Default::default()
        };
        let mesh = crate::mesh_cook::pack_cooked_mesh(&cpu, None).expect("pack");
        let payload = mesh.to_postcard().expect("postcard");
        let header = regia_asset::CookedHeader {
            format_version: regia_asset::COOKED_FORMAT_VERSION,
            type_tag: regia_asset::AssetTypeTag::mesh().0,
            asset_id: id,
            importer_version: 1,
            dependencies: Vec::new(),
            payload_len: payload.len() as u64,
        };
        regia_asset::encode_cooked_file(&header, &payload).expect("file")
    }

    /// A GPU-ready pak mesh keeps its submesh ranges (layout v2, ADR-2788): the ranges read back from the pak equal
    /// the loose file's, in index units, with the same material slots.
    #[test]
    fn a_gpu_ready_pak_mesh_keeps_its_submesh_ranges() {
        let id = AssetId::new_random();
        let loose = compact_mesh_file(id);
        let (header, payload) = regia_asset::decode_cooked_file(&loose).expect("loose");
        let mut mesh = regia_asset::CookedMesh::from_postcard(payload).expect("mesh");
        mesh.submeshes = vec![
            regia_asset::CookedSubmesh {
                index_byte_offset: 0,
                index_byte_length: 6,
                material_slot: 0,
            },
            regia_asset::CookedSubmesh {
                index_byte_offset: 6,
                index_byte_length: 6,
                material_slot: 1,
            },
        ];
        let payload = mesh.to_postcard().expect("postcard");
        let mut header = header;
        header.payload_len = payload.len() as u64;
        let loose = regia_asset::encode_cooked_file(&header, &payload).expect("file");

        let (packed, tag) = pak_payload(loose.clone(), regia_asset::AssetTypeTag::mesh().0, true, id);
        assert_eq!(tag, regia_asset::AssetTypeTag::gpu_mesh().0);
        let (_, gp) = regia_asset::decode_cooked_file(&packed).expect("packed");
        let view = regia_asset::GpuMeshView::parse(gp).expect("view");
        let want: Vec<(u32, u32, u32)> = mesh
            .submeshes
            .iter()
            .map(|s| (s.index_byte_offset / 2, s.index_byte_length / 2, s.material_slot))
            .collect();
        let got: Vec<(u32, u32, u32)> = view
            .submeshes
            .iter()
            .map(|s| (s.first_index, s.index_count, s.material_slot))
            .collect();
        assert_eq!(got, want);
        assert_eq!(got.len(), 2);
        assert_eq!(view.submesh_indices(1).expect("cut"), vec![0, 2, 3]);
        // Cooking twice is byte-identical.
        let (again, _) = pak_payload(loose.clone(), regia_asset::AssetTypeTag::mesh().0, true, id);
        assert_eq!(again, packed);
        // A misaligned range does not convert: the mesh stays compact.
        let mut odd = mesh.clone();
        odd.submeshes[0].index_byte_length = 5;
        let payload = odd.to_postcard().expect("postcard");
        header.payload_len = payload.len() as u64;
        let bad = regia_asset::encode_cooked_file(&header, &payload).expect("file");
        let (kept, tag) = pak_payload(bad.clone(), regia_asset::AssetTypeTag::mesh().0, true, id);
        assert_eq!(tag, regia_asset::AssetTypeTag::mesh().0);
        assert_eq!(kept, bad);
    }

    /// ADR-7919: a desktop pak opted in to GPU-ready meshes stores a `COLOR_0` mesh GPU-ready,
    /// and `load_mesh_bytes` of the pak entry returns the cooked colors. Since GPU mesh layout v3
    /// (ADR-10080) a skinned or morphing mesh is stored GPU-ready as well and keeps its skin and
    /// morph targets (with layout v2 it stayed compact).
    #[test]
    fn a_vertex_colored_mesh_is_stored_gpu_ready_and_keeps_its_colors() {
        let gpu = regia_asset::AssetTypeTag::gpu_mesh().0;
        let compact = regia_asset::AssetTypeTag::mesh().0;
        let id = AssetId::new_random();
        let file = |cpu: &regia_asset::CpuMesh| {
            let mesh = crate::mesh_cook::pack_cooked_mesh(cpu, None).expect("pack");
            let payload = mesh.to_postcard().expect("postcard");
            let header = regia_asset::CookedHeader {
                format_version: regia_asset::COOKED_FORMAT_VERSION,
                type_tag: compact,
                asset_id: id,
                importer_version: 1,
                dependencies: Vec::new(),
                payload_len: payload.len() as u64,
            };
            regia_asset::encode_cooked_file(&header, &payload).expect("file")
        };
        let base = regia_asset::CpuMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.3]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            indices: vec![0, 1, 2, 0, 2, 3],
            ..Default::default()
        };
        let mut colored = base.clone();
        colored.colors = vec![
            [1.0, 0.0, 0.0, 1.0],
            [0.0, 1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0, 1.0],
            [0.5; 4],
        ];
        let loose = file(&colored);
        let (packed, tag) = pak_payload(loose.clone(), compact, true, id);
        assert_eq!(tag, gpu, "a COLOR_0 mesh is stored GPU-ready");
        let want = regia_asset::load_mesh_bytes(&loose).expect("loose").mesh.colors;
        let got = regia_asset::load_mesh_bytes(&packed).expect("packed").mesh.colors;
        assert_eq!(got, want);
        assert_eq!(got.len(), 4);

        let mut skinned = colored.clone();
        skinned.skin = Some(regia_asset::CpuSkin {
            joint_names: vec!["root".into()],
            joints: vec![[0; 4]; 4],
            weights: vec![[65535, 0, 0, 0]; 4],
            ..Default::default()
        });
        let mut morphing = base;
        morphing.morph_targets = vec![regia_asset::CpuMorphTarget {
            name: "open".into(),
            vertex_indices: vec![2],
            position_deltas: vec![[0.0, 0.0, 0.1]],
            normal_deltas: Vec::new(),
            tangent_deltas: Vec::new(),
        }];
        // GPU mesh layout v3 (ADR-10080): skinned and morphing meshes are stored GPU-ready too,
        // and the pak entry loads with the loose file's skin and morph targets, also when the
        // submesh table is carried over (the second write).
        for mesh in [&skinned, &morphing] {
            let loose = file(mesh);
            let mut cooked = regia_asset::CookedMesh::from_postcard(
                regia_asset::decode_cooked_file(&loose).expect("wrapper").1,
            )
            .expect("compact");
            let width = match cooked.index_width {
                regia_asset::CookedIndexWidth::U16 => 2,
                regia_asset::CookedIndexWidth::U32 => 4,
            };
            cooked.submeshes = vec![
                regia_asset::CookedSubmesh {
                    index_byte_offset: 0,
                    index_byte_length: 3 * width,
                    material_slot: 0,
                },
                regia_asset::CookedSubmesh {
                    index_byte_offset: 3 * width,
                    index_byte_length: 3 * width,
                    material_slot: 1,
                },
            ];
            let (mut header, _) = regia_asset::decode_cooked_file(&loose).expect("wrapper");
            let payload = cooked.to_postcard().expect("postcard");
            header.payload_len = payload.len() as u64;
            let with_submeshes = regia_asset::encode_cooked_file(&header, &payload).expect("file");
            for loose in [loose, with_submeshes] {
                let (packed, tag) = pak_payload(loose.clone(), compact, true, id);
                assert_eq!(
                    tag, gpu,
                    "a skinned or morphing mesh is stored GPU-ready (GPU mesh v3)"
                );
                let want = regia_asset::load_mesh_bytes(&loose).expect("loose").mesh;
                let got = regia_asset::load_mesh_bytes(&packed).expect("packed").mesh;
                assert_eq!(got.skin, want.skin);
                assert_eq!(got.morph_targets, want.morph_targets);
                assert_eq!(got.colors, want.colors);
            }
        }
    }

    #[test]
    fn android_cache_dir_is_recognized_in_any_case() {
        let dir = std::path::Path::new("/p/.regia/cooked");
        assert!(!gpu_meshes_with(&dir.join("Android"), Some("1")));
        assert!(!gpu_meshes_with(&dir.join("ANDROID"), Some("1")));
        assert!(gpu_meshes_with(&dir.join("linux"), Some("1")));
    }

    /// Desktop paks opted in store meshes GPU-ready (never by default); an Android cache dir keeps them compact; the pak
    /// migrator turns an old compact pak into the GPU-ready one with every other byte kept.
    #[test]
    fn desktop_paks_store_gpu_ready_meshes_and_old_paks_migrate() {
        let tmp = regia_test_utils::unique_temp_dir("pak-gpu-meshes");
        let gpu = regia_asset::AssetTypeTag::gpu_mesh().0;
        let compact = regia_asset::AssetTypeTag::mesh().0;
        let mesh = AssetId::new_random();
        let other = AssetId::new_random();
        for platform in ["linux", "android"] {
            let cooked = tmp.join(platform);
            std::fs::create_dir_all(&cooked).expect("mkdir");
            std::fs::write(cooked.join(asset_cooked_file_name(mesh)), compact_mesh_file(mesh))
                .expect("write");
            std::fs::write(cooked.join(asset_cooked_file_name(other)), b"other bytes").expect("write");
            AssetManifest {
                entries: vec![
                    ManifestEntry {
                        path: "assets/quad.gltf#mesh/q/primitive_0".to_string(),
                        id: mesh,
                        type_path: "regia_asset::CookedMesh".to_string(),
                    },
                    ManifestEntry {
                        path: "assets/other.bin".to_string(),
                        id: other,
                        type_path: "regia_asset::CookedImage".to_string(),
                    },
                ],
            }
            .write_to_cache(&cooked)
            .expect("manifest");
            let out = tmp.join(format!("paks-{platform}"));
            let want = gpu_meshes_with(&cooked, Some("1"));
            assert!(!gpu_meshes_with(&cooked, None), "opt-in: off by default");
            let paks = pack_cooked_directory_with(&cooked, &out, &BTreeMap::new(), true, want).expect("pack");
            let reader = regia_asset::PakReader::open(&paks[0]).expect("open");
            let tag = reader.entry(mesh).expect("mesh").type_tag;
            if !want {
                assert_eq!(tag, compact, "{platform}");
            } else {
                assert_eq!(tag, gpu, "{platform}");
                let loaded = regia_asset::load_mesh_bytes(&reader.read_decompressed(mesh).expect("read"))
                    .expect("load");
                assert_eq!(loaded.mesh.indices, vec![0, 1, 2, 0, 2, 3]);
                assert_eq!(loaded.mesh.tangents.len(), 4);
            }
        }

        // The compact (`write_pak`) pak through the migrator.
        let cooked = tmp.join("linux");
        let old = tmp.join("old.fpak");
        let manifest = AssetManifest::read_from_cache(&cooked)
            .expect("read")
            .expect("manifest");
        let entries = [
            (mesh, compact),
            (other, TypePathHash::of("regia_asset::CookedImage")),
        ];
        write_pak(&old, &cooked, &entries, &manifest, &PakWriteOptions::default()).expect("write");
        let new = tmp.join("new.fpak");
        assert_eq!(migrate_pak_meshes(&old, &new, true).expect("migrate"), 1);
        let (old, new) = (
            regia_asset::PakReader::open(&old).expect("old"),
            regia_asset::PakReader::open(&new).expect("new"),
        );
        assert_eq!(new.entry(mesh).expect("mesh").type_tag, gpu);
        assert_eq!(new.toc.manifest, old.toc.manifest);
        assert_eq!(
            new.read_decompressed(other).expect("other"),
            old.read_decompressed(other).expect("other")
        );
        let before =
            regia_asset::load_mesh_bytes(&old.read_decompressed(mesh).expect("old")).expect("old mesh");
        let after =
            regia_asset::load_mesh_bytes(&new.read_decompressed(mesh).expect("new")).expect("new mesh");
        assert_eq!(before.mesh.positions, after.mesh.positions);
        assert_eq!(before.mesh.indices, after.mesh.indices);
        assert_eq!(
            regia_asset::verify_pak_file(tmp.join("new.fpak").as_path()).expect("verify"),
            regia_asset::PakIntegrity::Ok
        );
        // Migrating again keeps the one GPU-ready mesh as it is.
        assert_eq!(
            migrate_pak_meshes(&tmp.join("new.fpak"), &tmp.join("again.fpak"), true).expect("again"),
            1
        );
        cleanup(&tmp);
    }

    #[test]
    fn pack_cooked_directory_produces_pak() {
        let (tmp, cooked_dir, _entries) = make_cooked_dir("pcd");
        let out = tmp.join("paks");
        std::fs::create_dir_all(&out).expect("mkdir");

        let paks = pack_cooked_directory(&cooked_dir, &out, &BTreeMap::new(), true).expect("pack");
        assert_eq!(paks.len(), 1);
        assert!(paks[0].ends_with("base.fpak"));
        assert!(paks[0].is_file());
        cleanup(&tmp);
    }

    #[test]
    fn chunked_paks() {
        let (tmp, cooked_dir, _entries) = make_cooked_dir("chunk");
        let out = tmp.join("chunks");
        std::fs::create_dir_all(&out).expect("mkdir");

        let mut chunks = BTreeMap::new();
        chunks.insert(
            "meshes".to_string(),
            vec!["assets/mesh_0.glb".to_string(), "assets/mesh_1.glb".to_string()],
        );
        chunks.insert(
            "rest".to_string(),
            vec![
                "assets/mesh_2.glb".to_string(),
                "assets/mesh_3.glb".to_string(),
                "assets/mesh_4.glb".to_string(),
            ],
        );

        let paks = pack_cooked_directory(&cooked_dir, &out, &chunks, false).expect("pack");
        assert_eq!(paks.len(), 2);
        assert!(paks.iter().any(|p| p.to_string_lossy().contains("chunk_meshes")));
        assert!(paks.iter().any(|p| p.to_string_lossy().contains("chunk_rest")));
        cleanup(&tmp);
    }

    /// deep-zai-assets A8 (w9-pack-stamp-and-content): the `__unmatched` placeholder chunk is
    /// still not packed, but its assets, and assets no chunk selects, are counted and named
    /// instead of being dropped without a word.
    #[test]
    fn assets_in_no_pak_are_reported() {
        let (tmp, cooked_dir, _entries) = make_cooked_dir("skipped");
        let out = tmp.join("chunks");
        std::fs::create_dir_all(&out).expect("mkdir");
        let mut chunks = BTreeMap::new();
        chunks.insert(UNMATCHED_CHUNK.to_string(), vec!["assets/mesh_0.glb".to_string()]);
        chunks.insert(
            "rest".to_string(),
            vec!["assets/mesh_1.glb".to_string(), "assets/mesh_2.glb".to_string()],
        );
        let pack = pack_cooked_directory_report(&cooked_dir, &out, &chunks, false, false).expect("pack");
        assert_eq!(pack.paks.len(), 1, "the placeholder chunk gets no pak");
        assert!(pack.paks[0].ends_with("chunk_rest.fpak"));
        assert_eq!(
            pack.skipped,
            vec!["assets/mesh_0.glb", "assets/mesh_3.glb", "assets/mesh_4.glb"],
            "the placeholder's asset and the two no chunk selects"
        );
        // Without chunks everything is in base.fpak, so nothing is skipped.
        let base =
            pack_cooked_directory_report(&cooked_dir, &tmp.join("base"), &BTreeMap::new(), false, false)
                .expect("base");
        assert!(base.skipped.is_empty());
        // An asset two chunks select is counted once, as packed.
        chunks.insert("more".to_string(), vec!["assets/mesh_0.glb".to_string()]);
        let pack = pack_cooked_directory_report(&cooked_dir, &tmp.join("more"), &chunks, false, false)
            .expect("pack");
        assert_eq!(pack.skipped, vec!["assets/mesh_3.glb", "assets/mesh_4.glb"]);
        cleanup(&tmp);
    }

    #[test]
    fn glob_wildcard_matching() {
        assert!(simple_glob_match("assets/foo.glb", "**"));
        assert!(simple_glob_match("assets/foo.glb", "assets/**"));
        assert!(!simple_glob_match("assets/foo.glb", "textures/**"));
        assert!(simple_glob_match("assets/foo.glb", "assets/foo.glb"));
        assert!(!simple_glob_match("assets/foo.glb", "assets/bar.glb"));
        assert!(simple_glob_match("assets/foo.glb", "assets/*.glb"));
        assert!(!simple_glob_match("assets/sub/foo.glb", "assets/*.glb"));
        assert!(simple_glob_match("assets/sub/foo.glb", "assets/**/*.glb"));
        assert!(simple_glob_match("assets/mesh_1.glb", "assets/mesh_?.glb"));
    }

    /// A chunk written with a `*` pattern ships the assets it names; before, only `**`, a
    /// `dir/**` stem or an exact path matched and the chunk came out empty (no pak at all).
    #[test]
    fn star_patterns_select_chunk_members() {
        let (tmp, cooked_dir, _entries) = make_cooked_dir("star");
        let out = tmp.join("out");
        std::fs::create_dir_all(&out).expect("mkdir");
        let mut chunks = BTreeMap::new();
        chunks.insert("meshes".to_string(), vec!["assets/mesh_*.glb".to_string()]);
        let paks = pack_cooked_directory(&cooked_dir, &out, &chunks, false).expect("pack");
        assert_eq!(paks.len(), 1);
        assert!(paks[0].to_string_lossy().contains("chunk_meshes"));
        cleanup(&tmp);
    }

    #[test]
    fn dir_glob_needs_a_path_boundary() {
        assert!(simple_glob_match("props/hero.gltf", "props/**"));
        assert!(simple_glob_match("props/hero.gltf#mesh/a", "props/**"));
        assert!(simple_glob_match("props/sub/hero.gltf", "props/**"));
        assert!(simple_glob_match("props", "props/**"));
        assert!(!simple_glob_match("props_hd/hero.gltf", "props/**"));
        assert!(!simple_glob_match("propsx", "props/**"));
    }

    #[test]
    fn uncompressed_pak_is_readable() {
        let (tmp, cooked_dir, entries) = make_cooked_dir("unc");
        let out = tmp.join("out2");
        std::fs::create_dir_all(&out).expect("mkdir");

        let manifest = AssetManifest::read_from_cache(&cooked_dir)
            .expect("read")
            .expect("exists");
        write_pak(
            &out.join("base.fpak"),
            &cooked_dir,
            &entries,
            &manifest,
            &PakWriteOptions {
                compress: false,
                ..Default::default()
            },
        )
        .expect("write");

        let reader = regia_asset::PakReader::open(&out.join("base.fpak")).expect("open");
        assert!(!reader.header.is_compressed());
        cleanup(&tmp);
    }

    /// A highly-compressible payload (flat zeros / a solid-colour run) must round-trip through a
    /// `compress: true` pak: the writer detects the payload would exceed the reader's
    /// decompression-bomb ratio cap and stores it uncompressed, so `read_decompressed` returns the
    /// exact bytes instead of rejecting a pak it just wrote. This fails on the previous writer,
    /// which always stored `compress_prepend_size` output and left the reader to reject it.
    #[test]
    fn highly_compressible_payload_round_trips_through_compressed_pak() {
        // A solid RGBA8 pixel (4 bytes) repeated fills 256 KiB; lz4 collapses this to ~250:1.
        let solid_colour = {
            let mut v = Vec::with_capacity(256 * 1024);
            let pixel = [0x1Au8, 0x2Bu8, 0x3Cu8, 0xFFu8];
            for _ in 0..(256 * 1024 / 4) {
                v.extend_from_slice(&pixel);
            }
            v
        };
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("4096 zero bytes", vec![0u8; 4096]),
            ("256 KiB solid color", solid_colour),
        ];

        for (i, (label, payload)) in cases.iter().enumerate() {
            let (tmp, cooked_dir, id) = make_single_cooked_asset(&format!("hcp{i}"), payload);
            let out = tmp.join("out");
            std::fs::create_dir_all(&out).expect("mkdir");

            let manifest = AssetManifest::read_from_cache(&cooked_dir)
                .expect("read")
                .expect("exists");
            write_pak(
                &out.join("base.fpak"),
                &cooked_dir,
                &[(id, TypePathHash::of("regia_asset::CookedTexture"))],
                &manifest,
                &PakWriteOptions::default(),
            )
            .expect("write pak");

            let reader = regia_asset::PakReader::open(&out.join("base.fpak")).expect("open");
            let read = reader.read_decompressed(id).expect("read");
            assert_eq!(&read, payload, "{label}: round-trip must preserve bytes");
            cleanup(&tmp);
        }
    }

    /// A reordered entry list must give byte-identical paks: the writer sorts by asset id, so
    /// Steam-style chunk diffs between two cooks never move bytes just because a caller listed
    /// assets in a different order (spec 22 T2.14, ADR-3322).
    #[test]
    fn reordered_source_list_does_not_change_pak_bytes() {
        let (tmp, cooked_dir, entries) = make_cooked_dir("reorder");
        let manifest = AssetManifest::read_from_cache(&cooked_dir)
            .expect("read")
            .expect("exists");
        let first = tmp.join("first.fpak");
        let second = tmp.join("second.fpak");
        write_pak(
            &first,
            &cooked_dir,
            &entries,
            &manifest,
            &PakWriteOptions::default(),
        )
        .expect("write");
        let mut shuffled = entries.clone();
        shuffled.swap(0, 4);
        shuffled.swap(1, 3);
        write_pak(
            &second,
            &cooked_dir,
            &shuffled,
            &manifest,
            &PakWriteOptions::default(),
        )
        .expect("write");
        assert_eq!(
            std::fs::read(&first).expect("read"),
            std::fs::read(&second).expect("read"),
            "a reordered source list must not change pak bytes"
        );
        cleanup(&tmp);
    }

    /// Cook twice, edit one asset, cook again: every other asset keeps its bytes and its relative
    /// order in the TOC, so an update's patch touches only the edited asset's chunks plus the TOC
    /// (measured with the 1 MiB chunk model; ADR-3322).
    #[test]
    fn editing_one_asset_keeps_the_others_bytes_and_order() {
        let (tmp, cooked_dir, entries) = make_cooked_dir("edit-one");
        let manifest = AssetManifest::read_from_cache(&cooked_dir)
            .expect("read")
            .expect("exists");
        let old_pak = tmp.join("old.fpak");
        write_pak(
            &old_pak,
            &cooked_dir,
            &entries,
            &manifest,
            &PakWriteOptions::default(),
        )
        .expect("write");
        let edited = entries[2].0;

        // Same-length content edit: no layout shift at all.
        let same_len = tmp.join("same-len.fpak");
        let mut edited_payload = b"asset data 2".to_vec();
        edited_payload[7] = b'X';
        std::fs::write(cooked_dir.join(asset_cooked_file_name(edited)), edited_payload).expect("edit");
        write_pak(
            &same_len,
            &cooked_dir,
            &entries,
            &manifest,
            &PakWriteOptions::default(),
        )
        .expect("write");
        let old = regia_asset::PakReader::open(&old_pak).expect("open old");
        let same = regia_asset::PakReader::open(&same_len).expect("open same");
        let old_order: Vec<_> = old.toc.entries.iter().map(|e| e.id).collect();
        assert_eq!(
            old_order,
            same.toc.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            "the TOC order is stable across cooks"
        );
        for (id, _) in &entries {
            let before = old.read_decompressed(*id).expect("read old");
            let after = same.read_decompressed(*id).expect("read new");
            if *id == edited {
                assert_ne!(before, after, "the edited asset changed");
            } else {
                assert_eq!(before, after, "asset {id} keeps its bytes");
                assert_eq!(
                    old.entry(*id).expect("entry").offset,
                    same.entry(*id).expect("entry").offset,
                    "asset {id} keeps its offset"
                );
            }
        }
        // Measured patch: 4096-byte chunks, the edited asset + header + TOC differ, nothing else.
        let diff = regia_asset::pak_diff::diff_paks(&old_pak, &same_len, 4096).expect("diff");
        assert_eq!(diff.totals.changed, 1, "{diff:?}");
        assert_eq!(diff.totals.unchanged, 4, "{diff:?}");
        assert_eq!(diff.totals.moved, 0, "{diff:?}");

        // Growing edit: later assets move (same bytes, new offset), never change content.
        let grown = tmp.join("grown.fpak");
        std::fs::write(
            cooked_dir.join(asset_cooked_file_name(edited)),
            b"this payload grew by quite a few bytes",
        )
        .expect("edit");
        write_pak(
            &grown,
            &cooked_dir,
            &entries,
            &manifest,
            &PakWriteOptions::default(),
        )
        .expect("write");
        let grown_reader = regia_asset::PakReader::open(&grown).expect("open grown");
        assert_eq!(
            old_order,
            grown_reader.toc.entries.iter().map(|e| e.id).collect::<Vec<_>>(),
            "the TOC order is stable when an edit changes a size"
        );
        for (id, _) in &entries {
            if *id == edited {
                continue;
            }
            assert_eq!(
                old.read_decompressed(*id).expect("read old"),
                grown_reader.read_decompressed(*id).expect("read grown"),
                "asset {id} keeps its bytes when a sibling grows"
            );
        }
        let diff = regia_asset::pak_diff::diff_paks(&old_pak, &grown, 4096).expect("diff");
        assert_eq!(diff.totals.changed, 1, "{diff:?}");
        assert_eq!(diff.totals.unchanged + diff.totals.moved, 4, "{diff:?}");
        assert!(
            diff.assets
                .iter()
                .filter(|a| a.change == regia_asset::pak_diff::AssetChange::Moved)
                .all(|a| {
                    let o = old.entry(a.id).and_then(|e| old.read_raw(e.id).ok());
                    let n = grown_reader
                        .entry(a.id)
                        .and_then(|e| grown_reader.read_raw(e.id).ok());
                    o.as_deref() == n.as_deref()
                }),
            "moved assets carry their old bytes"
        );
        cleanup(&tmp);
    }

    /// Deterministic bytes lz4 cannot shrink, except that every `repeat_every`-th 256-byte
    /// segment repeats the one before it: lz4 saves about `1 / repeat_every` of the payload.
    fn payload_saving_about(len: usize, repeat_every: usize) -> Vec<u8> {
        const SEGMENT: usize = 256;
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut out = Vec::with_capacity(len + SEGMENT);
        let mut segment = 0usize;
        while out.len() < len {
            if segment.is_multiple_of(repeat_every) && out.len() >= SEGMENT {
                let start = out.len() - SEGMENT;
                out.extend_from_within(start..start + SEGMENT);
            } else {
                for _ in 0..SEGMENT {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    out.push((state >> 24) as u8);
                }
            }
            segment += 1;
        }
        out.truncate(len);
        out
    }

    /// A cooked directory of [`bc_and_mesh_cooked_dir`].
    struct RatioFixture {
        tmp: PathBuf,
        cooked: PathBuf,
        entries: Vec<(AssetId, TypePathHash)>,
        payloads: Vec<Vec<u8>>,
    }

    /// Two payloads in one cooked directory: one lz4 shrinks by about 8 % (a BC7 texture), one by
    /// about 33 % (a mesh).
    fn bc_and_mesh_cooked_dir(prefix: &str) -> RatioFixture {
        let tmp = regia_test_utils::unique_temp_dir(prefix);
        let cooked = tmp.join("cooked");
        std::fs::create_dir_all(&cooked).expect("mkdir");
        let payloads = vec![
            payload_saving_about(600_000, 12),
            payload_saving_about(600_000, 3),
        ];
        let mut entries = Vec::new();
        let mut manifest = AssetManifest::default();
        for (i, payload) in payloads.iter().enumerate() {
            let id = AssetId(regia_core::id::Guid::from_u128(i as u128 + 1));
            std::fs::write(cooked.join(asset_cooked_file_name(id)), payload).expect("write");
            entries.push((id, TypePathHash::of("regia_asset::CookedTexture")));
            manifest.entries.push(ManifestEntry {
                path: format!("assets/blob_{i}.ktx2"),
                id,
                type_path: "regia_asset::CookedTexture".to_string(),
            });
        }
        manifest.write_to_cache(&cooked).expect("write manifest");
        RatioFixture {
            tmp,
            cooked,
            entries,
            payloads,
        }
    }

    /// The stored fraction lz4 saved on each entry (0 for a raw entry).
    fn savings(reader: &regia_asset::PakReader, entries: &[(AssetId, TypePathHash)]) -> Vec<f64> {
        entries
            .iter()
            .map(|(id, _)| {
                let e = reader.toc.entries.iter().find(|e| e.id == *id).expect("entry");
                1.0 - e.compressed_size as f64 / e.size as f64
            })
            .collect()
    }

    /// DO27 COOKED_PACKS_DESIGN pak-min-ratio (ADR-7971): with `min_ratio = 0.15` a blob lz4
    /// shrinks by about 8 % is stored raw and `read_shared` serves it from the mapping, while a
    /// blob lz4 shrinks by about 33 % stays lz4; with `min_ratio = 0` (the default) both stay lz4.
    #[test]
    fn min_ratio_stores_a_blob_raw_when_lz4_saves_too_little() {
        let RatioFixture {
            tmp,
            cooked,
            entries,
            payloads,
        } = bc_and_mesh_cooked_dir("pak-min-ratio");
        let manifest = AssetManifest::read_from_cache(&cooked)
            .expect("read")
            .expect("exists");

        // Default: every blob lz4 shrinks at all is compressed (today's behavior).
        let any_shrink = tmp.join("any.fpak");
        write_pak(
            &any_shrink,
            &cooked,
            &entries,
            &manifest,
            &PakWriteOptions::default(),
        )
        .expect("write");
        let reader = regia_asset::PakReader::open(&any_shrink).expect("open");
        let saved = savings(&reader, &entries);
        assert!(
            (0.03..0.15).contains(&saved[0]) && saved[1] > 0.25,
            "fixture: lz4 must save 3-15 % of the first payload and over 25 % of the second, saved {saved:?}"
        );
        assert!(reader.toc.entries.iter().all(|e| e.block_table_offset != 0));

        let ship = tmp.join("ship.fpak");
        write_pak(
            &ship,
            &cooked,
            &entries,
            &manifest,
            &PakWriteOptions {
                min_ratio: SHIP_MIN_RATIO,
                ..PakWriteOptions::default()
            },
        )
        .expect("write");
        let reader = std::sync::Arc::new(regia_asset::PakReader::open(&ship).expect("open"));
        assert!(
            reader.header.is_compressed(),
            "the pak still compresses what pays"
        );
        let raw = reader.entry(entries[0].0).expect("raw entry");
        assert_eq!(
            (raw.block_table_offset, raw.compressed_size),
            (0, raw.size),
            "an 8 % saving is stored raw"
        );
        let lz4 = reader.entry(entries[1].0).expect("lz4 entry");
        assert_ne!(lz4.block_table_offset, 0, "a 33 % saving stays lz4");
        let shared = reader.read_shared(entries[0].0).expect("read raw");
        assert!(shared.is_mapped(), "the raw blob is a view of the mapping");
        assert_eq!(shared, payloads[0]);
        assert_eq!(
            reader.read_decompressed(entries[1].0).expect("read lz4"),
            payloads[1]
        );
        assert_eq!(
            regia_asset::verify_pak_file(&ship).expect("verify"),
            regia_asset::PakIntegrity::Ok
        );
        cleanup(&tmp);
    }

    /// The `min_ratio` rule at its edges: the threshold itself keeps lz4, out-of-range values clamp,
    /// NaN is 0, and lz4 output that is not smaller never stays compressed.
    #[test]
    fn min_ratio_threshold_edges() {
        assert!(keeps_compressed(85, 100, 0.15), "exactly 15 % saved keeps lz4");
        assert!(!keeps_compressed(86, 100, 0.15), "14 % saved is stored raw");
        assert!(keeps_compressed(99, 100, 0.0));
        assert!(keeps_compressed(99, 100, -1.0), "a negative ratio clamps to 0");
        assert!(keeps_compressed(99, 100, f32::NAN), "NaN reads as 0");
        assert!(!keeps_compressed(100, 100, 0.0), "not smaller: raw");
        assert!(!keeps_compressed(101, 100, 0.0));
        assert!(
            !keeps_compressed(1, 100, 2.0),
            "a ratio over 1 clamps to 1: every blob raw"
        );
        assert!(keeps_compressed(0, 100, 1.0));
    }
}
