// SPDX-License-Identifier: MPL-2.0
//! Alternative texture packages derived from cooked mip payloads (ADR-8059).
//!
//! The full-resolution paks and cooked cache remain unchanged. Each tier is a complete chunk
//! directory with the existing manifest schema. Hosts select one directory before mounting.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};
use ktx2::{Header, LevelIndex};
use regia_asset::content::{
    CHUNK_MAP_FILE, CHUNK_WHY_FILE, CONTENT_HOST_FILE, CONTENT_TRUST_FILE, ContentManifest, Delivery,
    RESUME_BLOCK, block_hashes, hash_file,
};
use regia_asset::{AssetId, AssetManifest, AssetTypeTag, CookedTexture, PakReader};

use crate::pak_writer::{PakWriteError, PakWriteOptions, write_pak_with};

/// One named texture package, parsed from `NAME=MAX_EDGE_PX` (`0` keeps the full chain).
/// Names are canonical uppercase ASCII, at most 40 characters, safe on every shipping platform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextureTier {
    name: String,
    max_edge_px: u32,
}

impl TextureTier {
    /// Canonical directory name, for example `LOW`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Requested upper bound on the retained mip edge, in texels; `0` means no reduction.
    #[must_use]
    pub fn max_edge_px(&self) -> u32 {
        self.max_edge_px
    }
}

impl FromStr for TextureTier {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid =
            || format!("texture_tier_invalid: expected NAME=MAX_EDGE_PX (0 = full chain), got {value:?}");
        let (name, edge) = value.split_once('=').ok_or_else(invalid)?;
        regia_asset::content::validate_chunk_name(&name.to_ascii_lowercase()).map_err(|_| invalid())?;
        if edge.is_empty() || !edge.bytes().all(|c| c.is_ascii_digit()) {
            return Err(invalid());
        }
        Ok(Self {
            name: name.to_ascii_uppercase(),
            max_edge_px: edge.parse().map_err(|_| invalid())?,
        })
    }
}

/// Validate a set before cooking or changing any output. Names compare case-insensitively.
///
/// # Errors
/// `texture_tier_duplicate` when a name occurs more than once.
pub fn validate_texture_tiers(tiers: &[TextureTier]) -> Result<()> {
    let mut names = BTreeSet::new();
    for tier in tiers {
        ensure!(
            names.insert(tier.name()),
            "texture_tier_duplicate: {}",
            tier.name()
        );
    }
    Ok(())
}

fn section(bytes: &[u8], offset: u64, length: u64) -> Result<&[u8]> {
    let start = usize::try_from(offset)?;
    let end = usize::try_from(offset.checked_add(length).context("KTX2 section overflow")?)?;
    bytes.get(start..end).context("KTX2 section outside file")
}

#[derive(Debug, thiserror::Error)]
#[error("texture_tier_missing_mip: cached chain has no mip at or under the requested edge")]
struct MissingMip;

/// Retain whole levels under the cap. `None` means the original file must be kept verbatim.
/// Layout follows Khronos KTX 2.0 §§ 3.9–3.13: index in logical order, payload smallest first,
/// DFD/KVD relocated unchanged, absent SGD zeroed, each level aligned to lcm(block bytes, 4).
fn trim_ktx2(bytes: &[u8], max_edge: u32) -> Result<Option<Vec<u8>>> {
    if let Some(reason) = regia_asset::hostile_ktx2_dfd(bytes) {
        bail!("texture_tier_ktx2: {reason}");
    }
    let reader = ktx2::Reader::new(bytes).context("texture_tier_ktx2")?;
    let mut header = reader.header();
    let edge = header
        .pixel_width
        .max(header.pixel_height)
        .max(header.pixel_depth);
    ensure!(edge > 0, "texture_tier_ktx2: zero dimensions");
    let count = header.level_count.max(1);
    ensure!(
        count <= 32 - edge.leading_zeros(),
        "texture_tier_ktx2: too many mip levels"
    );
    if max_edge == 0 || edge <= max_edge {
        return Ok(None);
    }
    ensure!(
        header.supercompression_scheme.is_none()
            && header.format.is_some()
            && header.index.sgd_byte_length == 0,
        "texture_tier_supercompressed: recook from source; the runtime needs a concrete format without supercompression"
    );
    let drop = (0..count)
        .find(|level| (edge >> level).max(1) <= max_edge)
        .ok_or(MissingMip)?;
    let basic = reader
        .basic_dfd()
        .context("texture_tier_ktx2: missing basic DFD")?;
    let block_bytes = usize::from(basic.bytes_planes[0]);
    ensure!(
        block_bytes != 0 && basic.bytes_planes[1..].iter().all(|b| *b == 0),
        "texture_tier_ktx2: unsupported multi-plane DFD"
    );
    // gcd(block_bytes, 4) is 4, 2 or 1.
    let divisor = if block_bytes.is_multiple_of(4) {
        4
    } else if block_bytes.is_multiple_of(2) {
        2
    } else {
        1
    };
    let alignment = block_bytes / divisor * 4;
    let source_index = section(
        bytes,
        Header::LENGTH as u64,
        u64::from(count) * LevelIndex::LENGTH as u64,
    )?;
    let mut levels: Vec<_> = source_index
        .as_chunks::<{ LevelIndex::LENGTH }>()
        .0
        .iter()
        .map(LevelIndex::from_bytes)
        .skip(drop as usize)
        .collect();
    let dfd = section(
        bytes,
        u64::from(header.index.dfd_byte_offset),
        u64::from(header.index.dfd_byte_length),
    )?;
    let kvd = if header.index.kvd_byte_length == 0 {
        &[][..]
    } else {
        section(
            bytes,
            u64::from(header.index.kvd_byte_offset),
            u64::from(header.index.kvd_byte_length),
        )?
    };
    let shrink = |size: u32| if size == 0 { 0 } else { (size >> drop).max(1) };
    header.pixel_width = shrink(header.pixel_width);
    header.pixel_height = shrink(header.pixel_height);
    header.pixel_depth = shrink(header.pixel_depth);
    header.level_count = count - drop;
    let mut output = vec![0; Header::LENGTH + levels.len() * LevelIndex::LENGTH];
    header.index.dfd_byte_offset = u32::try_from(output.len())?;
    output.extend_from_slice(dfd);
    header.index.kvd_byte_offset = if kvd.is_empty() {
        0
    } else {
        u32::try_from(output.len())?
    };
    output.extend_from_slice(kvd);
    header.index.sgd_byte_offset = 0;
    header.index.sgd_byte_length = 0;
    // Retained level bytes are never decoded, filtered or encoded again.
    for (level, index) in levels.iter_mut().enumerate().rev() {
        let payload = section(bytes, index.byte_offset, index.byte_length)?;
        let extent = [header.pixel_width, header.pixel_height, header.pixel_depth];
        let subresource_bytes = (block_bytes as u64)
            .checked_mul(u64::from(header.layer_count.max(1)))
            .and_then(|size| size.checked_mul(u64::from(header.face_count)))
            .context("texture_tier_ktx2: subresource size overflow")?;
        let expected = extent
            .iter()
            .zip(basic.texel_block_dimensions)
            .try_fold(subresource_bytes, |size, (edge, block)| {
                size.checked_mul(u64::from((edge >> level).max(1).div_ceil(u32::from(block.get()))))
            })
            .context("texture_tier_ktx2: image size overflow")?;
        ensure!(
            index.byte_length == expected && index.uncompressed_byte_length == expected,
            "texture_tier_ktx2: mip {} length does not match its block layout",
            level + drop as usize
        );
        output.resize(output.len().next_multiple_of(alignment), 0);
        index.byte_offset = output.len() as u64;
        output.extend_from_slice(payload);
    }
    output[..Header::LENGTH].copy_from_slice(&header.as_bytes());
    for (dest, level) in output[Header::LENGTH..Header::LENGTH + levels.len() * LevelIndex::LENGTH]
        .as_chunks_mut::<{ LevelIndex::LENGTH }>()
        .0
        .iter_mut()
        .zip(levels)
    {
        *dest = level.as_bytes();
    }
    ktx2::Reader::new(&output).context("texture_tier_ktx2: rewritten container")?;
    Ok(Some(output))
}

// Generated assets have their own residency/upload contracts. A material reference must
// never turn a terrain tile or proxy atlas into an ordinary capped material map.
fn uncapped_sub_asset(path: &str) -> bool {
    path.split_once('#').is_some_and(|(_, sub)| {
        sub.starts_with("terrain/") || sub.starts_with("hlod/") || sub.starts_with("voxels/")
    })
}

/// Find the slots passed to the runtime's material residency cap, across all chunks.
/// Unknown consumers stay full resolution. No cache or format additions are needed.
fn material_texture_ids(source: &Path) -> Result<BTreeSet<AssetId>> {
    let manifest = ContentManifest::read_from_dir(source)?
        .context("texture_tier_manifest: cook the chunk paks first")?;
    let paks = manifest
        .chunks
        .iter()
        .map(|chunk| PakReader::open(&source.join(&chunk.file)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut paths = BTreeMap::new();
    let mut uncapped = BTreeSet::new();
    for entry in paks.iter().flat_map(|pak| &pak.toc.manifest) {
        paths.entry(entry.path.as_str()).or_insert(entry.id);
        if uncapped_sub_asset(&entry.path) {
            uncapped.insert(entry.id);
        }
    }
    let mut maps = BTreeSet::new();
    let mut seen_materials = BTreeSet::new();
    for pak in &paks {
        for entry in &pak.toc.entries {
            if entry.type_tag != AssetTypeTag::standard_material().0
                || uncapped.contains(&entry.id)
                || !seen_materials.insert(entry.id)
            {
                continue;
            }
            let bytes = pak.read_decompressed(entry.id)?;
            let (_, material) = regia_asset::load_material_bytes(&bytes)?;
            let base = &material.authored.base;
            let detail = material
                .authored
                .hard_surface
                .as_ref()
                .and_then(|h| h.detail.as_ref());
            for slot in [
                base.base_color_map.as_ref(),
                base.normal_map.as_ref(),
                base.orm_map.as_ref(),
                base.emissive_map.as_ref(),
                detail.and_then(|d| d.albedo.as_ref()),
                detail.and_then(|d| d.normal.as_ref()),
            ]
            .into_iter()
            .flatten()
            {
                let id = match slot {
                    regia_asset::TextureSlot::Asset(id) => Some(*id),
                    regia_asset::TextureSlot::Path(path) => {
                        paths.get(path.replace('\\', "/").as_str()).copied()
                    }
                };
                if let Some(id) = id.filter(|id| !uncapped.contains(id)) {
                    maps.insert(id);
                }
            }
        }
    }
    Ok(maps)
}

fn tier_payload(bytes: Vec<u8>, cap: u32, warned: &mut BTreeSet<AssetId>) -> Result<Vec<u8>> {
    let (mut header, payload) = regia_asset::decode_cooked_file(&bytes)?;
    if header.type_tag != AssetTypeTag::texture().0 {
        return Ok(bytes);
    }
    let mut texture =
        CookedTexture::from_postcard(payload).context("texture_tier_ktx2: invalid cooked texture")?;
    if texture.usage == regia_asset::TextureCookUsage::Ui {
        return Ok(bytes);
    }
    if let Some(reason) = regia_asset::hostile_ktx2_dfd(&texture.ktx2) {
        bail!("texture_tier_ktx2: {reason}");
    }
    let reader = ktx2::Reader::new(&texture.ktx2).context("texture_tier_ktx2")?;
    let shape = reader.header();
    let basic = reader
        .basic_dfd()
        .context("texture_tier_ktx2: missing basic DFD")?;
    // HDR/float images and non-2D shapes do not use CompressedMaps' residency cap.
    if shape.face_count != 1
        || shape.layer_count != 0
        || shape.pixel_depth != 0
        || basic.sample_information.iter().any(|sample| {
            sample.channel_type_qualifiers.intersects(
                ktx2::dfd::ChannelTypeQualifiers::FLOAT | ktx2::dfd::ChannelTypeQualifiers::EXPONENT,
            )
        })
    {
        return Ok(bytes);
    }
    let ktx2 = match trim_ktx2(&texture.ktx2, cap) {
        Ok(Some(ktx2)) => ktx2,
        Ok(None) => return Ok(bytes),
        Err(error) if error.is::<MissingMip>() => {
            if warned.insert(header.asset_id) {
                tracing::warn!(
                    target: "regia_asset_pipeline",
                    asset_id = %header.asset_id,
                    max_edge_px = cap,
                    "texture_tier_missing_mip: no authored mip meets the cap; keeping the full cooked texture"
                );
            }
            return Ok(bytes);
        }
        Err(error) => return Err(error),
    };
    let dimensions = ktx2::Reader::new(&ktx2)?.header();
    texture.width = dimensions.pixel_width;
    texture.height = dimensions.pixel_height;
    texture.ktx2 = ktx2.into();
    let payload = texture.to_postcard()?;
    header.payload_len = payload.len() as u64;
    Ok(regia_asset::encode_cooked_file(&header, &payload)?)
}

fn copy_optional(source: &Path, destination: &Path) -> Result<()> {
    match std::fs::copy(source, destination) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !source.exists() => Ok(()),
        Err(e) => Err(e).with_context(|| format!("copying {}", source.display())),
    }
}

fn write_tier(
    source: &Path,
    out: &Path,
    tier: &TextureTier,
    (compress, min_ratio): (bool, f32),
    material_maps: &BTreeSet<AssetId>,
    warned: &mut BTreeSet<AssetId>,
) -> Result<Vec<PathBuf>> {
    let mut manifest = ContentManifest::read_from_dir(source)?
        .context("texture_tier_manifest: cook the chunk paks first")?;
    // Different downloads must not alias an HTTP build-id or installed chunk state. An
    // unlimited tier is the identical download, so its manifest also stays byte-identical.
    if tier.max_edge_px() != 0 {
        let identity = format!("{}\n{}\n{}", manifest.build_id, tier.name(), tier.max_edge_px());
        manifest.build_id = format!(
            "texture-{}-{}",
            tier.name(),
            blake3::hash(identity.as_bytes()).to_hex()
        );
        manifest.compatible_with.clear();
    }
    std::fs::create_dir_all(out)?;
    let mut written = Vec::new();
    for chunk in &mut manifest.chunks {
        let original = source.join(&chunk.file);
        ensure!(
            std::fs::metadata(&original)?.len() == chunk.size && hash_file(&original)? == chunk.hash,
            "texture_tier_manifest: source chunk {} differs from its manifest",
            chunk.name
        );
        let destination = out.join(&chunk.file);
        let pak = PakReader::open(&original)?;
        let texture_ids: BTreeSet<_> = pak
            .toc
            .entries
            .iter()
            .filter(|e| e.type_tag == AssetTypeTag::texture().0 && material_maps.contains(&e.id))
            .map(|e| e.id)
            .collect();
        let has_textures = !texture_ids.is_empty();
        let mut changed_sizes = BTreeMap::new();
        if tier.max_edge_px() == 0 || !has_textures {
            std::fs::copy(&original, &destination)?;
        } else {
            let entries: Vec<_> = pak.toc.entries.iter().map(|e| (e.id, e.type_tag)).collect();
            let toc = AssetManifest {
                entries: pak.toc.manifest.clone(),
            };
            write_pak_with(
                &destination,
                &entries,
                &toc,
                &PakWriteOptions {
                    compress,
                    min_ratio,
                    chunk_name: chunk.name.clone(),
                    dependencies: pak.toc.dependencies.clone(),
                },
                false,
                |id| {
                    let bytes = pak
                        .read_decompressed(id)
                        .map_err(|e| PakWriteError::Io(std::io::Error::other(e.to_string())))?;
                    // Non-texture entries may be arbitrary cooked payloads; do not interpret them.
                    if !texture_ids.contains(&id) {
                        return Ok(bytes);
                    }
                    let original_size = bytes.len();
                    let reduced = tier_payload(bytes, tier.max_edge_px(), warned)
                        .map_err(|e| PakWriteError::Io(std::io::Error::other(format!("{id}: {e:#}"))))?;
                    if reduced.len() != original_size {
                        changed_sizes.insert(id, reduced.len() as u64);
                    }
                    Ok(reduced)
                },
            )?;
        }
        let original_registry = regia_asset::registry::pak_registry_path(&original);
        let destination_registry = regia_asset::registry::pak_registry_path(&destination);
        if changed_sizes.is_empty() {
            copy_optional(&original_registry, &destination_registry)?;
        } else if let Some(registry) = regia_asset::registry::AssetRegistry::read_from(&original_registry)? {
            let entries = registry.entries().cloned().map(|mut entry| {
                if let Some(size) = changed_sizes.get(&entry.id) {
                    entry.size = *size;
                }
                entry
            });
            regia_asset::registry::AssetRegistry::from_entries(entries).write_to(&destination_registry)?;
        }
        chunk.size = std::fs::metadata(&destination)?.len();
        chunk.hash = hash_file(&destination)?;
        chunk.blocks = if chunk.delivery != Delivery::Install && chunk.size > RESUME_BLOCK {
            block_hashes(&mut std::io::BufReader::new(std::fs::File::open(&destination)?))?
        } else {
            Vec::new()
        };
        written.push(destination);
    }
    for name in [
        CHUNK_MAP_FILE,
        CHUNK_WHY_FILE,
        CONTENT_HOST_FILE,
        CONTENT_TRUST_FILE,
    ] {
        copy_optional(&source.join(name), &out.join(name))?;
    }
    if tier.max_edge_px() == 0 {
        let name = regia_asset::content::CONTENT_MANIFEST_FILE;
        std::fs::copy(source.join(name), out.join(name))?;
    } else {
        manifest.write_to_dir(out)?;
    }
    Ok(written)
}

/// Derive complete alternative chunk sets under `pak_dir/texture_tiers/NAME/`.
///
/// The ordinary chunk set must already be cooked. Ordinary 2D material maps retain only whole
/// mips at or below their tier edge, with the same asset ID. UI, HDR/float, cube/array/volume,
/// terrain VT, HLOD and voxel assets, and textures without a capped material slot stay verbatim.
/// Other entries, chunk membership, dependencies,
/// delivery rules and registries are preserved. No encoder runs and no source/cache is changed.
/// Reduced tiers have an existing-format content manifest with updated hashes and build identity;
/// unlimited tiers keep the original manifest identity as well as the original pak bytes.
/// A successful tier replaces its previous directory; a failure leaves the previous tier intact.
/// Empty `tiers` does no I/O. `compress` should match the ordinary cook's pak compression option.
/// A texture with no authored mip at or below the cap stays verbatim and emits
/// `texture_tier_missing_mip` once per asset ID for this call, across all chunks and tiers.
///
/// # Errors
/// Duplicate names, invalid cached KTX2 or material data, supercompression that
/// the runtime cannot load, a source pak inconsistent with its manifest, or filesystem errors.
pub fn write_texture_tier_paks(
    pak_dir: &Path,
    tiers: &[TextureTier],
    compress: bool,
) -> Result<Vec<PathBuf>> {
    write_texture_tier_paks_with(pak_dir, tiers, compress, 0.0)
}

/// [`write_texture_tier_paks`] with the ordinary cook's lz4 `min_ratio` (ADR-7971): a reduced
/// tier stores a blob raw exactly when the ordinary pak would (`regia cook --ship` passes
/// [`crate::pak_writer::SHIP_MIN_RATIO`]).
///
/// # Errors
/// As [`write_texture_tier_paks`].
pub fn write_texture_tier_paks_with(
    pak_dir: &Path,
    tiers: &[TextureTier],
    compress: bool,
    min_ratio: f32,
) -> Result<Vec<PathBuf>> {
    validate_texture_tiers(tiers)?;
    let material_maps = if tiers.iter().any(|tier| tier.max_edge_px() != 0) {
        material_texture_ids(pak_dir)?
    } else {
        BTreeSet::new()
    };
    let mut paks = Vec::new();
    let mut warned = BTreeSet::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for tier in tiers {
        let _span = tracing::info_span!(
            "texture_tier_paks",
            tier = tier.name(),
            max_edge_px = tier.max_edge_px()
        )
        .entered();
        let root = pak_dir.join("texture_tiers");
        let nonce = format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let stage = root.join(format!(".{}-{nonce}.staging", tier.name()));
        let backup = root.join(format!(".{}-{nonce}.old", tier.name()));
        let destination = root.join(tier.name());
        let result = (|| -> Result<()> {
            let written = write_tier(
                pak_dir,
                &stage,
                tier,
                (compress, min_ratio),
                &material_maps,
                &mut warned,
            )?;
            let had_previous = destination.exists();
            if had_previous {
                std::fs::rename(&destination, &backup)?;
            }
            if let Err(error) = std::fs::rename(&stage, &destination) {
                if had_previous {
                    std::fs::rename(&backup, &destination).context("restoring previous texture tier")?;
                }
                return Err(error.into());
            }
            if had_previous {
                std::fs::remove_dir_all(&backup)?;
            }
            for path in written {
                let name = path.file_name().context("tier pak without a file name")?;
                paks.push(destination.join(name));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&stage);
        }
        result.with_context(|| format!("texture tier {}", tier.name()))?;
    }
    Ok(paks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BcnFormat, Ktx2Layout, write_ktx2_bcn, write_ktx2_layered};

    fn chain(width: u32, height: u32, layout: Ktx2Layout) -> Vec<u8> {
        let count = 32 - width.max(height).leading_zeros();
        let mips: Vec<_> = (0..count)
            .map(|level| {
                let blocks = (width >> level).max(1).div_ceil(layout.block.0)
                    * (height >> level).max(1).div_ceil(layout.block.1)
                    * layout.layers.max(1)
                    * layout.faces;
                vec![level as u8; blocks as usize * layout.block.2]
            })
            .collect();
        write_ktx2_layered(width, height, layout, &mips).expect("fixture")
    }

    #[test]
    fn tier_ktx2_rebases_metadata_alignment_and_logical_mips() {
        let original = chain(4096, 2048, BcnFormat::Bc7Srgb.layout(0, 1).expect("layout"));
        let meta = regia_asset::MipStreamMeta {
            stream: false,
            min_resident_px: 128,
            ..regia_asset::MipStreamMeta::new(2048)
        };
        let original = crate::mip_stream::stamp_mip_stream(&original, &meta).expect("metadata");
        let original =
            crate::mip_stream::set_key_value(&original, "host.custom", b"kept verbatim\0").expect("key");
        let low = trim_ktx2(&original, 1024).expect("trim").expect("reduced");
        let source = ktx2::Reader::new(&original).expect("source");
        let reader = ktx2::Reader::new(&low).expect("tier");
        let header = reader.header();
        assert_eq!(
            (header.pixel_width, header.pixel_height, header.level_count),
            (1024, 512, 11)
        );
        assert_eq!(header.format, source.header().format);
        assert_eq!(
            (header.index.sgd_byte_offset, header.index.sgd_byte_length),
            (0, 0)
        );
        assert_eq!(regia_asset::MipStreamMeta::from_reader(&reader), Some(meta));
        assert_eq!(
            reader.key_value_data().collect::<Vec<_>>(),
            source.key_value_data().collect::<Vec<_>>()
        );
        assert_eq!(
            section(
                &low,
                header.index.dfd_byte_offset.into(),
                header.index.dfd_byte_length.into()
            )
            .expect("dfd"),
            section(
                &original,
                source.header().index.dfd_byte_offset.into(),
                source.header().index.dfd_byte_length.into()
            )
            .expect("original dfd")
        );
        assert_eq!(
            reader.levels().map(|l| l.data).collect::<Vec<_>>(),
            source.levels().skip(2).map(|l| l.data).collect::<Vec<_>>()
        );
        let indices: Vec<_> = low[Header::LENGTH..Header::LENGTH + 11 * LevelIndex::LENGTH]
            .as_chunks::<{ LevelIndex::LENGTH }>()
            .0
            .iter()
            .map(LevelIndex::from_bytes)
            .collect();
        assert!(indices.iter().all(|l| l.byte_offset.is_multiple_of(16)));
        assert!(
            indices.windows(2).all(|w| w[1].byte_offset < w[0].byte_offset),
            "physical order is smallest first"
        );
        let runtime = regia_render::CompressedTexture::from_ktx2(&low).expect("runtime loader");
        assert_eq!(
            (runtime.width, runtime.height, runtime.levels.len()),
            (1024, 512, 11)
        );
        assert_eq!(trim_ktx2(&original, 1024).expect("again"), Some(low.clone()));
        assert!(trim_ktx2(&low, 1024).expect("idempotent").is_none());
        assert_eq!(
            trim_ktx2(&low, 256).expect("staged"),
            trim_ktx2(&original, 256).expect("direct")
        );
    }

    #[test]
    fn tier_ktx2_preserves_small_and_unlimited_files_verbatim() {
        let file = chain(64, 32, BcnFormat::Bc5.layout(0, 1).expect("layout"));
        for cap in [0, 64, 1024, u32::MAX] {
            assert!(trim_ktx2(&file, cap).expect("unchanged").is_none());
        }
        let scratch = regia_test_utils::TempDir::new("texture-tier-noop");
        let root = scratch.join("not-created");
        assert!(
            write_texture_tier_paks(&root, &[], false)
                .expect("no flags")
                .is_empty()
        );
        assert!(!root.exists(), "empty tier list must do no IO");
    }

    #[test]
    fn tier_ktx2_handles_npot_arrays_cubes_and_all_cook_block_sizes() {
        for layout in [
            BcnFormat::Bc1Linear.layout(0, 1).expect("BC1"),
            BcnFormat::Bc4.layout(3, 1).expect("BC4 array"),
            BcnFormat::Bc7Linear.layout(2, 6).expect("BC7 cube array"),
            Ktx2Layout {
                format: ktx2::Format::ASTC_6x6_UNORM_BLOCK,
                block: (6, 6, 16),
                layers: 0,
                faces: 1,
            },
            Ktx2Layout {
                format: ktx2::Format::ETC2_R8G8B8_UNORM_BLOCK,
                block: (4, 4, 8),
                layers: 0,
                faces: 1,
            },
            Ktx2Layout {
                format: ktx2::Format::R8G8B8_UNORM,
                block: (1, 1, 3),
                layers: 0,
                faces: 1,
            },
        ] {
            let height = if layout.faces == 6 { 130 } else { 66 };
            let original = chain(130, height, layout);
            let reduced = trim_ktx2(&original, 63).expect("trim").expect("reduced");
            let full = ktx2::Reader::new(&original).expect("original");
            let low = ktx2::Reader::new(&reduced).expect("reduced");
            let header = low.header();
            assert_eq!((header.pixel_width, header.pixel_height), (32, height >> 2));
            assert_eq!(
                (header.layer_count, header.face_count),
                (layout.layers, layout.faces)
            );
            let alignment = if layout.block.2 == 3 {
                12
            } else {
                layout.block.2.max(4)
            };
            let index_end = Header::LENGTH + header.level_count as usize * LevelIndex::LENGTH;
            assert!(
                reduced[Header::LENGTH..index_end]
                    .as_chunks::<{ LevelIndex::LENGTH }>()
                    .0
                    .iter()
                    .map(LevelIndex::from_bytes)
                    .all(|l| l.byte_offset.is_multiple_of(alignment as u64))
            );

            assert_eq!(
                low.levels().map(|l| l.data).collect::<Vec<_>>(),
                full.levels().skip(2).map(|l| l.data).collect::<Vec<_>>()
            );
            let tiny = trim_ktx2(&original, 1).expect("sub-block cap").expect("tiny");
            let tiny = ktx2::Reader::new(&tiny).expect("tiny reader");
            assert_eq!(
                (
                    tiny.header().pixel_width,
                    tiny.header().pixel_height,
                    tiny.header().level_count
                ),
                (1, 1, 1)
            );
        }
    }

    #[test]
    fn tier_ktx2_refuses_a_missing_tail_and_supercompression_instead_of_ignoring_the_cap() {
        let truncated_chain =
            write_ktx2_bcn(64, 64, BcnFormat::Bc7Srgb, &[vec![0; 4096], vec![0; 1024]]).expect("short chain");
        let error = trim_ktx2(&truncated_chain, 16)
            .expect_err("no mip under cap")
            .to_string();
        assert!(error.contains("texture_tier_missing_mip"), "{error}");
        let mut file = chain(64, 64, BcnFormat::Bc7Srgb.layout(0, 1).expect("layout"));
        file[44..48].copy_from_slice(&2u32.to_le_bytes()); // Zstandard
        let error = trim_ktx2(&file, 16)
            .expect_err("unsupported runtime scheme")
            .to_string();
        assert!(error.contains("texture_tier_supercompressed"), "{error}");
    }

    #[test]
    fn tier_ktx2_malformed_sections_never_panic_or_publish_truncated_mips() {
        let file = chain(16, 16, BcnFormat::Bc7Srgb.layout(0, 1).expect("layout"));
        for length in 0..file.len() {
            // Physical order of this fixture ends with the smallest mip: every truncation is bad.
            assert!(trim_ktx2(&file[..length], 4).is_err(), "length {length}");
        }
        for (at, value) in [
            (40, u32::MAX),
            (48, u32::MAX),
            (52, u32::MAX),
            (56, u32::MAX),
            (60, u32::MAX),
        ] {
            let mut bad = file.clone();
            bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
            assert!(trim_ktx2(&bad, 4).is_err(), "header offset {at}");
        }
        let mut bad = file.clone();
        let dfd = ktx2::Reader::new(&file)
            .expect("reader")
            .header()
            .index
            .dfd_byte_offset as usize;
        bad[dfd + 16] = 0xff; // Hostile basic DFD block dimension, checked before dependency parser.
        assert!(trim_ktx2(&bad, 4).is_err());
        let mut bad = file;
        // Level 2 exists, but lies about its byte count.
        let length = Header::LENGTH + 2 * LevelIndex::LENGTH + 8;
        bad[length..length + 8].copy_from_slice(&1u64.to_le_bytes());
        assert!(trim_ktx2(&bad, 4).is_err());
    }

    #[test]
    fn texture_tier_names_are_portable_and_duplicates_are_rejected() {
        for input in ["LOW=1024", "high-2=2048", "Ultra=0"] {
            let tier: TextureTier = input.parse().expect("valid");
            assert_eq!(tier.name(), tier.name().to_ascii_uppercase());
        }
        for input in [
            "../LOW=1024",
            "a/b=2",
            "a\\b=2",
            "NUL=1024",
            "CON=0",
            "COM1=64",
            "=1",
            "LOW=-1",
            "LOW=",
            "LOW=+1",
            "LOW=4294967296",
            "lo.w=1",
            "é=1",
        ] {
            assert!(input.parse::<TextureTier>().is_err(), "{input}");
        }
        let tiers = ["LOW=1024".parse().expect("low"), "low=512".parse().expect("low")];
        assert!(
            validate_texture_tiers(&tiers)
                .expect_err("duplicate")
                .to_string()
                .contains("texture_tier_duplicate")
        );
    }
    fn texture_file(id: regia_asset::AssetId, edge: u32, levels: Option<usize>) -> Vec<u8> {
        let ktx = match levels {
            None => chain(edge, edge, BcnFormat::Bc7Srgb.layout(0, 1).expect("layout")),
            Some(count) => {
                let mips: Vec<_> = (0..count)
                    .map(|level| vec![level as u8; (edge >> level).max(1).div_ceil(4).pow(2) as usize * 16])
                    .collect();
                write_ktx2_bcn(edge, edge, BcnFormat::Bc7Srgb, &mips).expect("short chain")
            }
        };
        let texture = CookedTexture {
            width: edge,
            height: edge,
            usage: regia_asset::TextureCookUsage::ColorSrgb,
            ktx2: ktx.into(),
        };
        let payload = texture.to_postcard().expect("texture");
        regia_asset::encode_cooked_file(
            &regia_asset::CookedHeader {
                format_version: regia_asset::COOKED_FORMAT_VERSION,
                type_tag: AssetTypeTag::texture().0,
                asset_id: id,
                importer_version: 1000000,
                dependencies: vec![],
                payload_len: payload.len() as u64,
            },
            &payload,
        )
        .expect("cooked wrapper")
    }

    fn material_file(id: regia_asset::AssetId, texture: regia_asset::AssetId) -> Vec<u8> {
        let mut authored = regia_asset::MaterialAssetRon::default_standard();
        authored.base.base_color_map = Some(regia_asset::TextureSlot::Asset(texture));
        let payload = regia_asset::CookedStandardMaterial::new(authored)
            .to_postcard()
            .expect("material");
        regia_asset::encode_cooked_file(
            &regia_asset::CookedHeader {
                format_version: regia_asset::COOKED_FORMAT_VERSION,
                type_tag: AssetTypeTag::standard_material().0,
                asset_id: id,
                importer_version: 1000000,
                dependencies: vec![texture],
                payload_len: payload.len() as u64,
            },
            &payload,
        )
        .expect("material wrapper")
    }

    #[test]
    fn tier_paks_reduce_only_material_maps_and_preserve_uncapped_consumers() {
        use crate::chunks::{Assignment, ChunkRules, ChunkWriteOptions, write_chunks};
        use regia_asset::{AssetId, ManifestEntry, TextureCookUsage};

        let scratch = regia_test_utils::TempDir::new("texture-tier-consumers");
        let cache = scratch.join("cache");
        let out = scratch.join("paks");
        std::fs::create_dir_all(&cache).expect("cache");
        let mut manifest = AssetManifest::default();
        let mut assignment = Assignment::default();
        let mut preserved = Vec::new();
        let mut material_id = None;
        for (name, path, usage, layout, edge, referenced) in [
            (
                "material",
                "assets/material.png",
                TextureCookUsage::ColorSrgb,
                BcnFormat::Bc7Srgb.layout(0, 1).expect("BC7"),
                4096,
                true,
            ),
            (
                "ui",
                "assets/menu.png",
                TextureCookUsage::Ui,
                BcnFormat::Bc7Srgb.layout(0, 1).expect("BC7"),
                2048,
                true,
            ),
            (
                "hdr",
                "assets/sky.exr",
                TextureCookUsage::ColorLinear,
                Ktx2Layout {
                    format: ktx2::Format::R16G16B16A16_SFLOAT,
                    block: (1, 1, 8),
                    layers: 0,
                    faces: 1,
                },
                2048,
                true,
            ),
            (
                "cube",
                "assets/sky.dds",
                TextureCookUsage::ColorSrgb,
                BcnFormat::Bc7Srgb.layout(0, 6).expect("cube"),
                2048,
                true,
            ),
            (
                "hlod",
                "assets/map.level.ron#hlod/0/atlas/base",
                TextureCookUsage::ColorSrgb,
                BcnFormat::Bc7Srgb.layout(0, 1).expect("BC7"),
                2048,
                true,
            ),
            (
                "voxel_far",
                "assets/tree.gltf#voxels/tree/texture",
                TextureCookUsage::ColorSrgb,
                BcnFormat::Bc7Srgb.layout(0, 1).expect("BC7"),
                2048,
                true,
            ),
            (
                "terrain_vt",
                "assets/map.level.ron#terrain/albedo/mip0/tile_0_0",
                TextureCookUsage::ColorSrgb,
                BcnFormat::Bc7Srgb.layout(0, 1).expect("BC7"),
                256,
                true,
            ),
            (
                "unreferenced",
                "assets/lookup.png",
                TextureCookUsage::ColorSrgb,
                BcnFormat::Bc7Srgb.layout(0, 1).expect("BC7"),
                2048,
                false,
            ),
        ] {
            let id = AssetId::new_random();
            let texture = CookedTexture {
                width: edge,
                height: edge,
                usage,
                ktx2: chain(edge, edge, layout).into(),
            };
            let payload = texture.to_postcard().expect("texture");
            let (mut header, _) =
                regia_asset::decode_cooked_file(&texture_file(id, 4, None)).expect("header");
            header.payload_len = payload.len() as u64;
            let bytes = regia_asset::encode_cooked_file(&header, &payload).expect("wrapper");
            std::fs::write(cache.join(regia_asset::cooked_file_name(id)), &bytes).expect("texture file");
            manifest.entries.push(ManifestEntry {
                path: path.into(),
                id,
                type_path: "regia_asset::CookedTexture".into(),
            });
            assignment.chunk_of.insert(id, "textures".into());
            if referenced {
                let mat = AssetId::new_random();
                std::fs::write(
                    cache.join(regia_asset::cooked_file_name(mat)),
                    material_file(mat, id),
                )
                .expect("material file");
                manifest.entries.push(ManifestEntry {
                    path: format!("assets/{name}.material.ron"),
                    id: mat,
                    type_path: "regia_asset::CookedStandardMaterial".into(),
                });
                // Material references must be found across chunk boundaries.
                assignment.chunk_of.insert(mat, "base".into());
            }
            if name == "material" {
                material_id = Some(id);
            } else {
                preserved.push((name, id, bytes));
            }
        }
        manifest.write_to_cache(&cache).expect("manifest");
        write_chunks(
            &cache,
            &out,
            &assignment,
            &ChunkRules::default(),
            &ChunkWriteOptions {
                compress: false,
                build_id: "tier-consumers".into(),
                platform: "linux".into(),
                ..Default::default()
            },
        )
        .expect("paks");
        // Below the VT tile edge as well, so a mistaken tile reduction cannot hide.
        write_texture_tier_paks(&out, &["LOW=128".parse().expect("tier")], false).expect("tier paks");
        let low = PakReader::open(&out.join("texture_tiers/LOW/textures.fpak")).expect("tier");
        let changed: Vec<_> = preserved
            .iter()
            .filter_map(|(name, id, bytes)| {
                (low.read_decompressed(*id).expect("entry") != *bytes).then_some(*name)
            })
            .collect();
        assert!(changed.is_empty(), "uncapped textures changed: {changed:?}");
        let (_, reduced) = regia_asset::load_texture_bytes(
            &low.read_decompressed(material_id.expect("material id"))
                .expect("material texture"),
        )
        .expect("texture");
        assert_eq!((reduced.width, reduced.height), (128, 128));
    }

    #[test]
    fn tier_paks_keep_ids_chunk_rules_data_and_hashes_and_replace_only_complete_tiers() {
        use crate::chunks::{Assignment, ChunkRules, ChunkWriteOptions, write_chunks};
        use regia_asset::{AssetId, ManifestEntry};
        use std::collections::BTreeMap;

        let scratch = regia_test_utils::TempDir::new("texture-tier-paks");
        let root = scratch.path().to_path_buf();
        let cache = root.join("cache");
        let out = root.join("paks");
        std::fs::create_dir_all(&cache).expect("cache");
        let large = AssetId::new_random();
        let small = AssetId::new_random();
        let data = AssetId::new_random();
        let material = AssetId::new_random();
        let manifest = AssetManifest {
            entries: vec![
                ManifestEntry {
                    path: "assets/large.png".into(),
                    id: large,
                    type_path: "regia_asset::CookedTexture".into(),
                },
                ManifestEntry {
                    path: "assets/small.png".into(),
                    id: small,
                    type_path: "regia_asset::CookedTexture".into(),
                },
                ManifestEntry {
                    path: "assets/data.bin".into(),
                    id: data,
                    type_path: "regia_asset::CookedData".into(),
                },
                ManifestEntry {
                    path: "assets/large.material.ron".into(),
                    id: material,
                    type_path: "regia_asset::CookedStandardMaterial".into(),
                },
            ],
        };
        manifest.write_to_cache(&cache).expect("cache manifest");
        let large_file = texture_file(large, 4096, None);
        let small_file = texture_file(small, 64, None);
        std::fs::write(cache.join(regia_asset::cooked_file_name(large)), &large_file).expect("large");
        std::fs::write(cache.join(regia_asset::cooked_file_name(small)), &small_file).expect("small");
        std::fs::write(
            cache.join(regia_asset::cooked_file_name(material)),
            material_file(material, large),
        )
        .expect("material");
        std::fs::write(
            cache.join(regia_asset::cooked_file_name(data)),
            b"arbitrary non-texture bytes",
        )
        .expect("data");
        let assignment = Assignment {
            chunk_of: BTreeMap::from([
                (large, "textures".into()),
                (small, "base".into()),
                (data, "textures".into()),
                (material, "textures".into()),
            ]),
            ..Default::default()
        };
        let rules = ChunkRules::from_manifest_toml("[cook.chunks.textures]\ninclude = [\"large.png\"]\ndelivery = \"external\"\nrequires = [\"base\"]\n").expect("rules");
        let options = ChunkWriteOptions {
            compress: false,
            build_id: "original-build".into(),
            platform: "linux".into(),
            ..Default::default()
        };
        let (full_manifest, _) =
            write_chunks(&cache, &out, &assignment, &rules, &options).expect("ordinary paks");
        let base = std::fs::read(out.join("base.fpak")).expect("base");
        let full = std::fs::read(out.join("textures.fpak")).expect("full");
        let low: TextureTier = "LOW=1024".parse().expect("tier");
        let ultra = "ULTRA=0".parse().expect("full tier");
        write_texture_tier_paks(&out, &[low.clone(), ultra], false).expect("tiers");
        assert_eq!(
            std::fs::read(
                out.join("texture_tiers/ULTRA")
                    .join(regia_asset::content::CONTENT_MANIFEST_FILE)
            )
            .expect("ULTRA manifest"),
            std::fs::read(out.join(regia_asset::content::CONTENT_MANIFEST_FILE)).expect("ordinary manifest")
        );
        let selected = out.join("texture_tiers/LOW");
        let low_pak = std::fs::read(selected.join("textures.fpak")).expect("LOW");
        assert_eq!(
            std::fs::read(selected.join("base.fpak")).expect("small base"),
            base
        );
        assert_eq!(
            std::fs::read(out.join("texture_tiers/ULTRA/textures.fpak")).expect("ULTRA"),
            full
        );
        assert_eq!(
            std::fs::read(cache.join(regia_asset::cooked_file_name(large))).expect("cache unchanged"),
            large_file
        );
        assert_eq!(
            std::fs::read(out.join("textures.fpak")).expect("ordinary unchanged"),
            full
        );
        let tier_manifest = ContentManifest::read_from_dir(&selected)
            .expect("read")
            .expect("manifest");
        assert_ne!(tier_manifest.build_id, full_manifest.build_id);
        let tier_chunk = tier_manifest.chunk("textures").expect("chunk");
        let full_chunk = full_manifest.chunk("textures").expect("full chunk");
        assert_eq!(
            (tier_chunk.delivery, &tier_chunk.requires, tier_chunk.mount_order),
            (full_chunk.delivery, &full_chunk.requires, full_chunk.mount_order)
        );
        assert_eq!(
            tier_chunk.hash,
            hash_file(&selected.join(&tier_chunk.file)).expect("hash")
        );
        assert_eq!(
            tier_chunk.blocks,
            block_hashes(&mut std::io::Cursor::new(&low_pak)).expect("block hashes")
        );
        assert!(!tier_chunk.blocks.is_empty());
        assert_eq!(
            std::fs::read(selected.join(CHUNK_MAP_FILE)).expect("map"),
            std::fs::read(out.join(CHUNK_MAP_FILE)).expect("original map")
        );
        let reader = PakReader::open(&selected.join("textures.fpak")).expect("pak");
        assert_eq!(
            reader.read_decompressed(data).expect("data"),
            b"arbitrary non-texture bytes"
        );
        let (_, reduced) = regia_asset::load_texture_bytes(&reader.read_decompressed(large).expect("entry"))
            .expect("runtime loader");
        assert_eq!((reduced.width, reduced.height), (1024, 1024));
        let runtime = regia_render::CompressedTexture::from_cooked(&reduced).expect("renderer loader");
        let (_, original) = regia_asset::load_texture_bytes(&large_file).expect("original");
        assert_eq!(
            runtime.levels,
            regia_render::CompressedTexture::from_cooked(&original)
                .expect("original levels")
                .levels[2..]
        );
        drop(reader);

        // A repeated cook is deterministic and replaces obsolete files in this tier.
        std::fs::write(selected.join("obsolete.fpak"), b"old chunk").expect("obsolete");
        write_texture_tier_paks(&out, std::slice::from_ref(&low), false).expect("repeat");
        assert!(!selected.join("obsolete.fpak").exists());
        assert_eq!(
            std::fs::read(selected.join("textures.fpak")).expect("repeat"),
            low_pak
        );
        let before =
            std::fs::read(selected.join(regia_asset::content::CONTENT_MANIFEST_FILE)).expect("tier manifest");
        // Missing tails now stay verbatim. Corrupt KTX2 still must fail without replacing
        // the old tier; retain the same pak/manifest/staging preservation assertions.
        let (mut bad_header, mut bad_texture) = regia_asset::load_texture_bytes(&large_file).expect("source");
        bad_texture.ktx2 = vec![0; Header::LENGTH].into();
        let bad_payload = bad_texture.to_postcard().expect("bad texture");
        bad_header.payload_len = bad_payload.len() as u64;
        std::fs::write(
            cache.join(regia_asset::cooked_file_name(large)),
            regia_asset::encode_cooked_file(&bad_header, &bad_payload).expect("corrupt container wrapper"),
        )
        .expect("corrupt source");
        write_chunks(&cache, &out, &assignment, &rules, &options).expect("new source paks");
        let error = write_texture_tier_paks(&out, &[low], false).expect_err("invalid KTX2");
        let error = format!("{error:#}");
        assert!(error.contains("texture_tier_ktx2"), "{error}");
        assert_eq!(
            std::fs::read(selected.join("textures.fpak")).expect("previous pak"),
            low_pak
        );
        assert_eq!(
            std::fs::read(selected.join(regia_asset::content::CONTENT_MANIFEST_FILE))
                .expect("previous manifest"),
            before
        );
        assert!(
            std::fs::read_dir(out.join("texture_tiers"))
                .expect("tiers")
                .all(|e| !e.expect("entry").file_name().to_string_lossy().starts_with('.'))
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
