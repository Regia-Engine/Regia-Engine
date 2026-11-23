// SPDX-License-Identifier: MPL-2.0
//! Photoreal terrain DATA-path cook: tiled 16-bit heightfields and tiled macro/layer textures for
//! Direct Order 27's 4096² heightfields and 8K maps.
//!
//! This module is the *cook* half of the terrain data lane (the runtime half is
//! `regia_terrain::stream`). It turns source images into a virtual-texture layout:
//!
//! * **Heightfield** — the 4096² R16 samples are kept as 16-bit, split into `tile_size` square
//!   tiles, and each tile carries its own `[min, max]` sample so LOD bounds are available without
//!   scanning the whole map ([`cook_tiled_heightfield`]).
//! * **Textures** — 8K macro maps (albedo / normal / AO), 2 × RGBA 8K splat masks and 6 material
//!   layers × (albedo / normal / ORM / height) are split into `tile_size` tiles at every mip level
//!   at or above the tile size, each tile compressed into its own KTX2 with a full sub-mip chain
//!   ([`cook_tiled_texture`]). Mips smaller than the tile are packed into a single resident "tail"
//!   KTX2.
//!
//! Output is deterministic: the same source bytes produce the same cooked bytes (asset ids are
//! derived with [`tile_asset_id`] from a stable parent id, never a random UUID). For Android ASTC
//! tiles that holds on one host ISA: `ctt-astcenc` builds astcenc with `ASTCENC_NO_INVARIANCE`
//! and picks SSE2/SSE4.1/AVX2 at run time on x86_64 (NEON on aarch64), so cook hosts of
//! different ISAs may write different Android tile bytes (ADR-5294). Desktop block compression
//! uses the same `intel_tex_2` encoders as `crate::texture_compress`. Android tiles
//! are ASTC 4×4 LDR (Khronos Data Format Specification 1.3, chapter 23) from the Android texture
//! cook's encoder (vendored ARM astcenc, `texture_compress`), with the terrain arrays' channel
//! layouts (ADR-5294). Independent tiles encode on scoped threads; the output order is fixed.
//!
//! Budgets (per-map cooked size and per-profile GPU residency) are computed by
//! [`terrain_map_budget`] and surfaced through `regia cook --stats`.

use std::path::Path;

use regia_asset::{
    AssetId, COOKED_FORMAT_VERSION, CookedHeader, CookedTexture, Heightfield, TextureCookUsage,
    cooked_file_name, encode_cooked_file,
};
use thiserror::Error;
use tracing::instrument;

use crate::texture_compress::{TextureCookQuality, cook_ktx2_from_mips, generate_mips_rgba8};

/// Square tile edge length used for the virtual-texture page grid.
pub const DEFAULT_TILE_SIZE: u32 = 256;

/// ASTC block edge of the Android tiled cook, in texels.
///
/// `docs/formats/terrain.md` § 7 names 4×4: the smallest block-compressible level, and the
/// footprint of the desktop BCn blocks. The physical layout is the Khronos Data Format
/// Specification 1.3, chapter 23 (128-bit ASTC blocks, including void-extent constant blocks).
pub const ASTC_BLOCK_EDGE: u32 = 4;

/// Version of the tiled-heightfield cooked format (see `docs/formats/terrain.md` § 8). The tiled
/// textures themselves are standard [`regia_asset::CookedTexture`] KTX2 assets (versioned by the
/// cooked-texture wrapper); the tile layout is a code-level contract, not a separately serialised
/// file.
pub const TILED_HEIGHTFIELD_FORMAT_VERSION: u32 = 1;

/// Importer version stamped into every terrain-data `.fckd` cooked file (packed `1.0.0`).
///
/// Distinct from the texture/glTF importers so a terrain-data cook can be invalidated independently;
/// the runtime streamer ignores it and keys residency off the deterministic asset ids instead.
/// `1_000_000` is `pack_importer_version(1, 0, 0)`.
pub const TERRAIN_DATA_IMPORTER_VERSION: u32 = 1_000_000;

/// Errors produced by the terrain data cooker.
#[derive(Debug, Error)]
pub enum TerrainDataError {
    /// A source image is smaller than the smallest block-compressible size (4×4).
    #[error("texture {width}x{height} is too small to block-compress")]
    TooSmall {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },

    /// The source image is not square (terrain maps are square).
    #[error("terrain textures must be square, got {width}x{height}")]
    NotSquare {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },

    /// The tile size and the map size must be powers of two, the tile dividing the map.
    #[error(
        "tile size {tile_size} and the map size {map_size} must both be powers of two, the tile dividing the map"
    )]
    BadTileSize {
        /// The requested tile size.
        tile_size: u32,
        /// The source map size.
        map_size: u32,
    },

    /// The RGBA buffer does not match the declared dimensions.
    #[error("rgba buffer length {len} does not match {width}x{height}")]
    BadBuffer {
        /// Buffer byte length.
        len: usize,
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
    },

    /// Texture compression failed (BCn/ASTC).
    #[error("texture compression failed: {0}")]
    Compress(#[from] crate::texture_compress::TextureCookError),

    /// A tile edge is not a multiple of the ASTC 4×4 block. Raised before any encode.
    #[error("terrain_astc_alignment: tile {tile_size} is not a multiple of the ASTC {block} block")]
    AstcAlignment {
        /// Requested tile edge, pixels.
        tile_size: u32,
        /// ASTC block edge, pixels.
        block: u32,
    },

    /// The ASTC encoder or the KTX2 packer rejected a mip (`terrain_astc_encode`).
    #[error("terrain_astc_encode: {0}")]
    AstcEncode(String),

    /// The heightfield sample buffer does not match its declared dimensions.
    #[error("heightfield samples length {len} does not match {width}x{height}")]
    BadHeightfield {
        /// Sample buffer length.
        len: usize,
        /// Width.
        width: u32,
        /// Height.
        height: u32,
    },

    /// A tile/tail KTX2 payload could not be parsed.
    #[error("invalid ktx2 payload: {0}")]
    BadKtx2(String),

    /// Serialising the cooked-texture wrapper failed.
    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),

    /// Encoding the `.fckd` cooked wrapper failed.
    #[error("cooked file error: {0}")]
    Cooked(#[from] regia_asset::CookedIoError),

    /// Writing cooked terrain files to disk failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The block-compressed format of a tiled texture's tiles.
///
/// The choice per slot follows `docs/specs/26-terrain-vfx-highend.md` and the frozen texture-usage
/// mapping in `crate::texture_compress`:
///
/// * **Albedo** → [`TileFormat::Bc7Srgb`] (8 bpp, 4-channel sRGB colour).
/// * **Normal** → [`TileFormat::Bc5`] (8 bpp, RG two-channel).
/// * **AO / height** → [`TileFormat::Bc4`] (4 bpp, single channel).
/// * **Masks / ORM** → [`TileFormat::Bc7Linear`] (8 bpp, 4-channel linear).
///
/// The RGBA splat masks use BC7-linear rather than a per-channel-group BC1/BC4 split: four channels
/// at BC4 is 16 bpp (4 × 4 bpp), while one BC7 texture carries all four channels at 8 bpp — half the
/// bytes for the same four channels, with three-subset partitions and 8-bit endpoints that preserve
/// smooth weight gradients. Quality is measured as the deterministic bit rate (see
/// [`tiled_texture_bytes`]); there is no golden-image comparison (the shading lane's render goldens
/// are not part of this lane) and no decoder-based PSNR (`intel_tex_2` is encode-only). The coverage
/// test below instead verifies each tile's VkFormat and that its texels map to the correct source
/// region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileFormat {
    /// BC7 sRGB (colour albedo).
    Bc7Srgb,
    /// BC5 two-channel (normal RG).
    Bc5,
    /// BC4 single channel (AO, height).
    Bc4,
    /// BC7 linear (4-channel masks, ORM).
    Bc7Linear,
}

impl TileFormat {
    /// The [`TextureCookUsage`] this tile format compresses with.
    pub fn usage(self) -> TextureCookUsage {
        match self {
            TileFormat::Bc7Srgb => TextureCookUsage::ColorSrgb,
            TileFormat::Bc5 => TextureCookUsage::NormalMap,
            TileFormat::Bc4 => TextureCookUsage::Mask,
            TileFormat::Bc7Linear => TextureCookUsage::ColorLinear,
        }
    }
}

/// One cooked tile: a mip level, its grid position, and the KTX2 bytes (full sub-mip chain).
#[derive(Clone, Debug, PartialEq)]
pub struct TileCook {
    /// Mip level of the source texture this tile belongs to.
    pub mip: u32,
    /// Tile column within the mip's tile grid.
    pub x: u32,
    /// Tile row within the mip's tile grid.
    pub y: u32,
    /// KTX2 bytes of this tile, compressed and with its own mip chain.
    pub ktx2: Vec<u8>,
}

/// The complete cooked output of a single tiled texture: its metadata, its tail and every tile.
#[derive(Clone, Debug, PartialEq)]
pub struct TiledTextureCook {
    /// Stable slot name (`"macro_albedo"`, `"mask0"`, `"layer2_orm"`, …).
    pub name: String,
    /// Source width (mip 0), pixels.
    pub width: u32,
    /// Source height (mip 0), pixels.
    pub height: u32,
    /// Tile edge length, pixels.
    pub tile_size: u32,
    /// Block-compressed format of the tiles.
    pub format: TileFormat,
    /// Total mip levels of the source texture (`floor(log2(width)) + 1`).
    pub mip_count: u32,
    /// Mip levels that live in the tail (the first mip smaller than `tile_size`, down to 4×4).
    pub tail_mips: u32,
    /// KTX2 bytes of the resident tail (mips below `tile_size`).
    pub tail_ktx2: Vec<u8>,
    /// Every tile, ordered by `(mip, y, x)` (row-major within each mip).
    pub tiles: Vec<TileCook>,
}

/// Cook a square RGBA8 image into a tiled virtual texture with a resident tail.
///
/// `width`/`height` must be equal powers of two and `tile_size` must divide them. The fine mips
/// (`size >= tile_size`) are split into `tile_size` tiles, each compressed to its own KTX2 with a
/// full sub-mip chain; the mips smaller than the tile are packed into one tail KTX2 (down to 4×4,
/// the smallest block-compressible level). `platform == "android"` writes the same tiles as ASTC
/// 4×4 LDR ([`ASTC_BLOCK_EDGE`]). A tile edge that is not a multiple of that block is refused
/// with [`TerrainDataError::AstcAlignment`] before any encode. Sub-mips of 2×2 and 1×1 pad to one
/// block, as the desktop chain does.
///
/// # Errors
///
/// See [`TerrainDataError`].
#[instrument(skip_all, fields(slot = %name, w = width, h = height, tile = tile_size, platform = %platform))]
#[allow(clippy::too_many_arguments)] // a self-contained cook entry point
pub fn cook_tiled_texture(
    name: &str,
    width: u32,
    height: u32,
    rgba: &[u8],
    tile_size: u32,
    format: TileFormat,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<TiledTextureCook, TerrainDataError> {
    if width != height {
        return Err(TerrainDataError::NotSquare { width, height });
    }
    if width < 4 {
        return Err(TerrainDataError::TooSmall { width, height });
    }
    if platform == "android" && !tile_size.is_multiple_of(ASTC_BLOCK_EDGE) {
        return Err(TerrainDataError::AstcAlignment {
            tile_size,
            block: ASTC_BLOCK_EDGE,
        });
    }
    if !tile_size.is_power_of_two() || !width.is_power_of_two() || !width.is_multiple_of(tile_size) {
        return Err(TerrainDataError::BadTileSize {
            tile_size,
            map_size: width,
        });
    }
    let expected = width as usize * height as usize * 4;
    if rgba.len() != expected {
        return Err(TerrainDataError::BadBuffer {
            len: rgba.len(),
            width,
            height,
        });
    }

    let mips = generate_mips_rgba8(width, height, rgba);
    let mip_count = mips.len() as u32;

    // Tiled mips: size >= tile_size. Each tile becomes its own full-mip-chain texture.
    let tail_start = (width / tile_size).ilog2() + 1;
    // Jobs name a tile of a level of `mips`; each worker copies its tile out when it encodes it,
    // so the cook holds one tile per worker, never a second copy of the tiled levels.
    let mut jobs = Vec::new();
    for (mip, (mw, mh, data)) in mips.iter().enumerate() {
        debug_assert_eq!(*mw, *mh);
        if *mw < tile_size {
            break;
        }
        let tiles_per_side = *mw / tile_size;
        for y in 0..tiles_per_side {
            for x in 0..tiles_per_side {
                jobs.push(LevelTile {
                    mip: mip as u32,
                    x,
                    y,
                    level: data,
                    level_size: *mw,
                });
            }
        }
    }
    let tiles = encode_tiles(&jobs, tile_size, format, platform, quality)?;

    // Tail mips: the first mip smaller than tile_size, down to 4x4 (block-compressible minimum).
    let tail_mips_len = tail_mip_count(width, tile_size);
    let tail_mips: Vec<(u32, u32, Vec<u8>)> = mips
        .iter()
        .skip(tail_start as usize)
        .take(tail_mips_len as usize)
        .cloned()
        .collect();
    let tail_first = tail_mips.first().map(|(w, _, _)| *w).unwrap_or(1);
    let tail_ktx2 = cook_chain(tail_first, tail_first, &tail_mips, format, platform, quality)?;

    Ok(TiledTextureCook {
        name: name.to_string(),
        width,
        height,
        tile_size,
        format,
        mip_count,
        tail_mips: tail_mips_len,
        tail_ktx2,
        tiles,
    })
}

/// Most threads [`encode_tiles`] uses (the cap the banded BCn encoder uses too).
const TILE_THREADS_MAX: usize = 16;

/// One tile to encode: its `(mip, x, y)` grid coordinates and its RGBA8 texels.
trait TileJob: Sync {
    /// `(mip, x, y)` of the tile.
    fn coords(&self) -> (u32, u32, u32);
    /// The tile's `tile_size`×`tile_size` RGBA8 texels.
    fn texels(&self, tile_size: u32) -> std::borrow::Cow<'_, [u8]>;
}

/// A tile of one level of the cook's shared mip chain, copied out only when it is encoded.
struct LevelTile<'a> {
    mip: u32,
    x: u32,
    y: u32,
    level: &'a [u8],
    level_size: u32,
}

impl TileJob for LevelTile<'_> {
    fn coords(&self) -> (u32, u32, u32) {
        (self.mip, self.x, self.y)
    }

    fn texels(&self, tile_size: u32) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Owned(extract_tile(
            self.level_size,
            tile_size,
            self.level,
            self.x,
            self.y,
        ))
    }
}

/// An already-extracted tile (`(mip, x, y, rgba)`), as the tests build them.
#[cfg(test)]
impl TileJob for (u32, u32, u32, Vec<u8>) {
    fn coords(&self) -> (u32, u32, u32) {
        (self.0, self.1, self.2)
    }

    fn texels(&self, _tile_size: u32) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(&self.3)
    }
}

/// Encodes every tile job (in grid order) to its own mip-chain KTX2. Tiles are independent, so
/// contiguous runs of jobs encode on scoped threads (at most [`TILE_THREADS_MAX`]) and the
/// results are joined in job order: the output and its bytes are the same as a serial loop's,
/// and an error is the first failing tile's in that order (ADR-5294: the Android ASTC tile cook
/// ran on one core). See [`run_jobs_in_order`] for the inner encoders' thread budget.
fn encode_tiles<J: TileJob>(
    jobs: &[J],
    tile_size: u32,
    format: TileFormat,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<Vec<TileCook>, TerrainDataError> {
    let encode = |job: &J| -> Result<TileCook, TerrainDataError> {
        let (mip, x, y) = job.coords();
        let ktx2 = cook_chain(
            tile_size,
            tile_size,
            &generate_mips_rgba8(tile_size, tile_size, &job.texels(tile_size)),
            format,
            platform,
            quality,
        )?;
        Ok(TileCook { mip, x, y, ktx2 })
    };
    let threads = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(TILE_THREADS_MAX)
        .min(jobs.len());
    run_jobs_in_order(jobs, threads, encode)
}

/// Runs `f` over `jobs` on `threads` scoped threads (contiguous runs), joined in job order; one
/// thread runs them inline. A threaded run caps each worker's inner block encoders at one band
/// (`texture_compress::with_band_thread_cap`), so a large tile does not spawn up to 16 band
/// threads per worker on top of the workers (at most `threads` OS threads, not `threads × 16`).
/// The bytes do not change: banded and one-call encodes are identical. A serial run keeps the
/// encoders' own banding.
fn run_jobs_in_order<J: Sync, T: Send, E: Send>(
    jobs: &[J],
    threads: usize,
    f: impl Fn(&J) -> Result<T, E> + Sync,
) -> Result<Vec<T>, E> {
    if threads <= 1 {
        return jobs.iter().map(f).collect();
    }
    let run = jobs.len().div_ceil(threads);
    let f = &f;
    let runs: Vec<Vec<Result<T, E>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .chunks(run)
            .map(|chunk| {
                scope.spawn(move || {
                    crate::texture_compress::with_band_thread_cap(1, || chunk.iter().map(f).collect())
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic)))
            .collect()
    });
    runs.into_iter().flatten().collect()
}

/// Compress one mip chain. Desktop calls the existing BCn cooker unchanged. Android goes through
/// the Android texture cook's astcenc path at 4×4 ([`cook_chain_astc`]). Partial levels (2×2,
/// 1×1) become one block, and the KTX2 writer counts `div_ceil` blocks.
fn cook_chain(
    width: u32,
    height: u32,
    mips: &[(u32, u32, Vec<u8>)],
    format: TileFormat,
    platform: &str,
    quality: TextureCookQuality,
) -> Result<Vec<u8>, TerrainDataError> {
    if platform == "android" {
        return cook_chain_astc(width, height, mips, format);
    }
    cook_ktx2_from_mips(width, height, mips, format.usage(), platform, quality)
        .map_err(TerrainDataError::from)
}

/// ASTC 4×4 LDR chain through the Android texture cook's encoder (vendored ARM astcenc,
/// `astc_compress`), with the channel layouts the Android terrain material arrays use: sRGB
/// albedo, linear RGB(A) masks and ORM, one-channel AO and height (`R = G = B`), and normals in
/// `.rg` like BC5 (`R G 0 1`). Always 4×4 (§ 7), never the low-quality 6×6 / 8×8 ladder.
/// Medium preset (the Android texture cook's) for every slot but normals, which take Thorough:
/// ADR-5294 measured `.rg` normals at Medium exactly 1 dB under BC5. A level whose buffer is not
/// `w × h × 4` bytes is [`TerrainDataError::AstcEncode`], never a panic.
fn cook_chain_astc(
    width: u32,
    height: u32,
    mips: &[(u32, u32, Vec<u8>)],
    format: TileFormat,
) -> Result<Vec<u8>, TerrainDataError> {
    use crate::astc_compress::{AstcContent, compress_astc_rgba8_preset};
    use ctt_astcenc::Preset;
    for (w, h, rgba) in mips {
        let expected = *w as usize * *h as usize * 4;
        if rgba.len() != expected {
            return Err(TerrainDataError::AstcEncode(format!(
                "a {w}x{h} level has {} bytes, expected {expected}",
                rgba.len()
            )));
        }
    }
    let srgb = matches!(format, TileFormat::Bc7Srgb);
    let has_alpha = || {
        mips.first()
            .is_some_and(|(_, _, px)| px.as_chunks::<4>().0.iter().any(|c| c[3] < 255))
    };
    let (content, preset) = match format {
        TileFormat::Bc7Srgb | TileFormat::Bc7Linear if has_alpha() => (AstcContent::Rgba, Preset::Medium),
        TileFormat::Bc7Srgb | TileFormat::Bc7Linear => (AstcContent::Rgb, Preset::Medium),
        TileFormat::Bc4 => (AstcContent::Single, Preset::Medium),
        TileFormat::Bc5 => (AstcContent::Rg, Preset::Thorough),
    };
    let levels = mips
        .iter()
        .map(|(w, h, rgba)| {
            compress_astc_rgba8_preset(*w, *h, rgba, (4, 4), srgb, content, preset)
                .map_err(|err| TerrainDataError::AstcEncode(err.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    crate::ktx2_write::write_ktx2_astc(
        width,
        height,
        crate::ktx2_write::AstcBlock::Block4x4,
        srgb,
        &levels,
    )
    .map_err(|err| TerrainDataError::AstcEncode(err.to_string()))
}

/// Number of tail mips for a `size`×`size` texture tiled at `tile_size`: the first mip smaller
/// than `tile_size` down to 4×4 (the smallest block-compressible level).
fn tail_mip_count(size: u32, tile_size: u32) -> u32 {
    let tail_start = (size / tile_size).ilog2() + 1;
    let last = size.ilog2() - 2;
    if tail_start <= last {
        last - tail_start + 1
    } else {
        0
    }
}

/// Copies one `tile_size`×`tile_size` tile out of a `size`×`size` RGBA8 mip level.
fn extract_tile(size: u32, tile_size: u32, mip: &[u8], tx: u32, ty: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((tile_size * tile_size * 4) as usize);
    for row in 0..tile_size {
        let src_row = (ty * tile_size + row) as usize;
        let src_start = (src_row * size as usize + (tx * tile_size) as usize) * 4;
        let src_end = src_start + (tile_size * 4) as usize;
        out.extend_from_slice(&mip[src_start..src_end]);
    }
    out
}

// ---------------------------------------------------------------------------
// Tiled heightfield
// ---------------------------------------------------------------------------

/// One tile of a tiled heightfield: its 16-bit samples plus per-tile `[min, max]` for LOD bounds.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HeightfieldTile {
    /// Tile column.
    pub x: u32,
    /// Tile row.
    pub y: u32,
    /// Lowest sample value in the tile (inclusive).
    pub min: u16,
    /// Highest sample value in the tile (inclusive).
    pub max: u16,
    /// Row-major `tile_size`² 16-bit samples (kept 16-bit; no resampling).
    pub samples: Vec<u16>,
}

/// A cooked tiled 16-bit heightfield with per-tile `[min, max]` bounds (see `docs/formats/terrain.md` § 8).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TiledHeightfield {
    /// Format version; loaders reject unknown versions.
    pub version: u32,
    /// Width in samples.
    pub width: u32,
    /// Height in samples.
    pub height: u32,
    /// Tile edge length in samples.
    pub tile_size: u32,
    /// Tiles ordered by `(x, y)`, row-major.
    pub tiles: Vec<HeightfieldTile>,
}

impl TiledHeightfield {
    /// Serialises to postcard bytes for the cooked cache / pak.
    ///
    /// # Errors
    ///
    /// Returns a postcard error only for implausibly large inputs.
    pub fn to_postcard(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }
}

/// Split a 16-bit heightfield into `tile_size` square tiles, each with its own `[min, max]`.
///
/// The samples are copied verbatim (16-bit preserved; no resampling or smoothing — spec 26 § 1
/// requires the render surface to match the simulation's samples). The last tile along an edge is
/// padded with the border sample when the map size is not a multiple of the tile size.
///
/// # Errors
///
/// Returns [`TerrainDataError::BadTileSize`] if `tile_size` is not a power of two, or
/// [`TerrainDataError::BadHeightfield`] if the sample buffer does not match the dimensions.
pub fn cook_tiled_heightfield(
    heightfield: &Heightfield,
    tile_size: u32,
) -> Result<TiledHeightfield, TerrainDataError> {
    if !tile_size.is_power_of_two() {
        return Err(TerrainDataError::BadTileSize {
            tile_size,
            map_size: heightfield.width.max(heightfield.height),
        });
    }
    let (w, h) = (heightfield.width, heightfield.height);
    let expected = w as usize * h as usize;
    if heightfield.samples.len() != expected {
        return Err(TerrainDataError::BadHeightfield {
            len: heightfield.samples.len(),
            width: w,
            height: h,
        });
    }

    let tiles_x = w.div_ceil(tile_size);
    let tiles_y = h.div_ceil(tile_size);
    let mut tiles = Vec::with_capacity((tiles_x * tiles_y) as usize);
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            tiles.push(build_heightfield_tile(tx, ty, tile_size, w, h, |col, row| {
                heightfield.samples[row as usize * w as usize + col as usize]
            }));
        }
    }
    Ok(TiledHeightfield {
        version: TILED_HEIGHTFIELD_FORMAT_VERSION,
        width: w,
        height: h,
        tile_size,
        tiles,
    })
}

/// One cooked tile from a sample lookup: the tile's `tile_size`² samples with coordinates past
/// the map clamped to its last row/column (the border padding of [`cook_tiled_heightfield`]).
/// Both the whole-grid and the region-fed cook build every tile here, so their bytes agree.
fn build_heightfield_tile(
    tx: u32,
    ty: u32,
    tile_size: u32,
    w: u32,
    h: u32,
    sample: impl Fn(u32, u32) -> u16,
) -> HeightfieldTile {
    let mut min = u16::MAX;
    let mut max = 0u16;
    let mut samples = Vec::with_capacity((tile_size * tile_size) as usize);
    for row in 0..tile_size {
        let src_row = (ty * tile_size + row).min(h - 1);
        for col in 0..tile_size {
            let src_col = (tx * tile_size + col).min(w - 1);
            let v = sample(src_col, src_row);
            min = min.min(v);
            max = max.max(v);
            samples.push(v);
        }
    }
    HeightfieldTile {
        x: tx,
        y: ty,
        min,
        max,
        samples,
    }
}

/// The part of the map one cooked tile reads in [`cook_tiled_heightfield_regions`]: columns
/// `x0..x1` and rows `y0..y1` (exclusive ends, already clipped to the map).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeightfieldRegion {
    /// Tile column.
    pub tx: u32,
    /// Tile row.
    pub ty: u32,
    /// First map column.
    pub x0: u32,
    /// First map row.
    pub y0: u32,
    /// End map column (exclusive).
    pub x1: u32,
    /// End map row (exclusive).
    pub y1: u32,
}

/// What a region callback of [`cook_tiled_heightfield_regions`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionFill {
    /// The buffer holds the region's samples, row-major; cook the tile from them.
    Filled,
    /// Reuse this tile from the `previous` cook unchanged (its inputs did not change).
    Reuse,
}

/// [`cook_tiled_heightfield`] fed one tile region at a time instead of a whole-map buffer
/// (ADR-8595): `fill` receives each region in row-major tile order and either fills the buffer
/// with exactly its samples or asks to reuse the tile from `previous`. The tiles, padding and
/// bytes equal [`cook_tiled_heightfield`] of the same samples; only one region buffer is held.
///
/// # Errors
///
/// [`TerrainDataError::BadTileSize`] / [`TerrainDataError::BadHeightfield`] as the whole-map cook
/// (a filled buffer of the wrong length, a reuse without a matching previous tile, a map under
/// one sample), or `fill`'s own error.
pub fn cook_tiled_heightfield_regions<E: From<TerrainDataError>>(
    width: u32,
    height: u32,
    tile_size: u32,
    previous: Option<&TiledHeightfield>,
    mut fill: impl FnMut(HeightfieldRegion, &mut Vec<u16>) -> Result<RegionFill, E>,
) -> Result<TiledHeightfield, E> {
    if !tile_size.is_power_of_two() {
        return Err(TerrainDataError::BadTileSize {
            tile_size,
            map_size: width.max(height),
        }
        .into());
    }
    if width == 0 || height == 0 {
        return Err(TerrainDataError::BadHeightfield {
            len: 0,
            width,
            height,
        }
        .into());
    }
    let previous = previous.filter(|p| {
        p.version == TILED_HEIGHTFIELD_FORMAT_VERSION
            && [p.width, p.height, p.tile_size] == [width, height, tile_size]
    });
    let tiles_x = width.div_ceil(tile_size);
    let tiles_y = height.div_ceil(tile_size);
    let mut tiles = Vec::with_capacity((tiles_x * tiles_y) as usize);
    let mut buffer = Vec::new();
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let region = HeightfieldRegion {
                tx,
                ty,
                x0: tx * tile_size,
                y0: ty * tile_size,
                x1: ((tx + 1) * tile_size).min(width),
                y1: ((ty + 1) * tile_size).min(height),
            };
            match fill(region, &mut buffer)? {
                RegionFill::Reuse => {
                    let reused = previous
                        .and_then(|p| p.tiles.get((ty * tiles_x + tx) as usize))
                        .filter(|t| [t.x, t.y] == [tx, ty]);
                    let Some(reused) = reused else {
                        return Err(TerrainDataError::BadHeightfield {
                            len: 0,
                            width,
                            height,
                        }
                        .into());
                    };
                    tiles.push(reused.clone());
                }
                RegionFill::Filled => {
                    let rw = (region.x1 - region.x0) as usize;
                    let len = rw * (region.y1 - region.y0) as usize;
                    if buffer.len() != len {
                        return Err(TerrainDataError::BadHeightfield {
                            len: buffer.len(),
                            width: region.x1 - region.x0,
                            height: region.y1 - region.y0,
                        }
                        .into());
                    }
                    let samples = &buffer;
                    tiles.push(build_heightfield_tile(
                        tx,
                        ty,
                        tile_size,
                        width,
                        height,
                        |col, row| samples[(row - region.y0) as usize * rw + (col - region.x0) as usize],
                    ));
                }
            }
        }
    }
    Ok(TiledHeightfield {
        version: TILED_HEIGHTFIELD_FORMAT_VERSION,
        width,
        height,
        tile_size,
        tiles,
    })
}

// ---------------------------------------------------------------------------
// Deterministic asset ids
// ---------------------------------------------------------------------------

/// The asset-path string of a tile, shared with the runtime stream module (`docs/formats/terrain.md` § 7).
///
/// The runtime derives the same string, so a tile's cooked `AssetId` agrees on both sides.
pub fn tile_asset_path(slot: &str, mip: u32, x: u32, y: u32) -> String {
    format!("terrain/{slot}/mip{mip}/tile_{x}_{y}")
}

/// The deterministic [`AssetId`] of a tile, derived from the map id and the tile path.
///
/// Using [`AssetId::sub_asset`] (a BLAKE3 keyed hash) rather than a random UUID keeps the cook
/// reproducible: the same map + slot + tile always yields the same id on every machine.
pub fn tile_asset_id(map_id: AssetId, slot: &str, mip: u32, x: u32, y: u32) -> AssetId {
    AssetId::sub_asset(map_id, &tile_asset_path(slot, mip, x, y))
}

/// The asset-path string of a texture's resident tail (shared with the runtime stream module).
///
/// The tail is a single KTX2 covering the coarse mips below the tile size; it has its own id so the
/// streamer can request it independently of any tile (`docs/formats/terrain.md` § 7).
pub fn tail_asset_path(slot: &str) -> String {
    format!("terrain/{slot}/tail")
}

/// The deterministic [`AssetId`] of a texture's resident tail, derived from the map id and slot.
pub fn tail_asset_id(map_id: AssetId, slot: &str) -> AssetId {
    AssetId::sub_asset(map_id, &tail_asset_path(slot))
}

// ---------------------------------------------------------------------------
// Cooked `.fckd` output (cook/pak path)
// ---------------------------------------------------------------------------

/// Encodes one KTX2 payload (a tile or the resident tail) as a `CookedTexture` `.fckd` file: a
/// cooked header (type tag `regia_asset::CookedTexture`) followed by the postcard-serialised
/// [`CookedTexture`]. The `width`/`height` are read from the KTX2 header so the wrapper always
/// agrees with the payload. This is the exact format `regia_terrain::stream::PakTileSource::load`
/// decodes with `regia_asset::load_texture_bytes`, so cooker and streamer agree by contract.
pub(crate) fn encode_cooked_texture(
    id: AssetId,
    usage: TextureCookUsage,
    ktx2: &[u8],
) -> Result<Vec<u8>, TerrainDataError> {
    let reader = ktx2::Reader::new(ktx2).map_err(|e| TerrainDataError::BadKtx2(e.to_string()))?;
    let cooked = CookedTexture {
        width: reader.header().pixel_width,
        height: reader.header().pixel_height,
        usage,
        ktx2: ktx2.into(),
    };
    let payload = cooked.to_postcard()?;
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_asset::AssetTypeTag::texture().0,
        asset_id: id,
        importer_version: TERRAIN_DATA_IMPORTER_VERSION,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    Ok(encode_cooked_file(&header, &payload)?)
}

/// Writes a cooked tiled texture — the resident tail plus every tile — as `CookedTexture` `.fckd`
/// files under `out_dir`, named by their deterministic asset ids ([`tail_asset_id`] /
/// [`tile_asset_id`]). Returns the `(asset id, byte length)` of every file written, tail first.
///
/// # Errors
///
/// Returns [`TerrainDataError`] if a KTX2 payload is malformed or the files cannot be written.
pub fn write_tiled_texture_cooked(
    map_id: AssetId,
    cooked: &TiledTextureCook,
    out_dir: &Path,
) -> Result<Vec<(AssetId, u64)>, TerrainDataError> {
    std::fs::create_dir_all(out_dir)?;
    let usage = cooked.format.usage();
    let mut written = Vec::with_capacity(cooked.tiles.len() + 1);

    let tail_id = tail_asset_id(map_id, &cooked.name);
    let tail_file = encode_cooked_texture(tail_id, usage, &cooked.tail_ktx2)?;
    crate::cook_ledger::write_atomic(&out_dir.join(cooked_file_name(tail_id)), &tail_file)?;
    written.push((tail_id, tail_file.len() as u64));

    for tile in &cooked.tiles {
        let id = tile_asset_id(map_id, &cooked.name, tile.mip, tile.x, tile.y);
        let file = encode_cooked_texture(id, usage, &tile.ktx2)?;
        crate::cook_ledger::write_atomic(&out_dir.join(cooked_file_name(id)), &file)?;
        written.push((id, file.len() as u64));
    }
    Ok(written)
}

/// The reflect type path of a cooked tiled heightfield (used as its `.fckd` type tag). The runtime
/// CDLOD lane adds a decoder for this type; until then only the cooker reads it back.
pub const TILED_HEIGHTFIELD_TYPE_PATH: &str = "regia_asset_pipeline::terrain_data::TiledHeightfield";

/// The asset-path string of a map's tiled heightfield (shared with the runtime stream module).
pub fn heightfield_asset_path() -> String {
    "terrain/heightfield".to_string()
}

/// The deterministic [`AssetId`] of a map's tiled heightfield, derived from the map id.
pub fn heightfield_asset_id(map_id: AssetId) -> AssetId {
    AssetId::sub_asset(map_id, &heightfield_asset_path())
}

/// Writes a cooked tiled heightfield as a `.fckd` file under `out_dir`, named by
/// [`heightfield_asset_id`]. Returns the `(asset id, byte length)` of the file written.
///
/// # Errors
///
/// Returns [`TerrainDataError`] if the file cannot be serialised or written.
pub fn write_tiled_heightfield_cooked(
    map_id: AssetId,
    tiled: &TiledHeightfield,
    out_dir: &Path,
) -> Result<(AssetId, u64), TerrainDataError> {
    std::fs::create_dir_all(out_dir)?;
    let id = heightfield_asset_id(map_id);
    let payload = tiled.to_postcard()?;
    let header = CookedHeader {
        format_version: COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(TILED_HEIGHTFIELD_TYPE_PATH),
        asset_id: id,
        importer_version: TERRAIN_DATA_IMPORTER_VERSION,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    let file = encode_cooked_file(&header, &payload)?;
    crate::cook_ledger::write_atomic(&out_dir.join(cooked_file_name(id)), &file)?;
    Ok((id, file.len() as u64))
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

/// A rendering profile and its GPU tile-cache budget (see `docs/evidence/terrain-data/budgets.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderProfile {
    /// Human-readable profile name.
    pub name: &'static str,
    /// GPU memory budget for resident terrain tiles, bytes.
    pub tile_budget_bytes: u64,
}

/// The three GPU profiles the terrain data path targets (spec 26 acceptance, DO27 floor/mid tiers).
///
/// The budget is the *resident tile cache* allowance per profile (a fraction of VRAM / unified
/// memory), not the total device memory. The full per-map footprint is small enough that streaming
/// (LRU eviction) only engages when multiple maps and future detail layers exceed the budget.
pub const RENDER_PROFILES: &[RenderProfile] = &[
    RenderProfile {
        name: "RTX 5090",
        tile_budget_bytes: 8 * 1024 * 1024 * 1024,
    },
    RenderProfile {
        name: "RX 7900 XTX",
        tile_budget_bytes: 6 * 1024 * 1024 * 1024,
    },
    RenderProfile {
        name: "Steam Deck (Medium)",
        tile_budget_bytes: 1024 * 1024 * 1024,
    },
];

/// Exact BCn mip-chain byte size for a `size`×`size` square texture (mips down to 4×4, the smallest
/// block-compressible level), summing `block_bytes` per 4×4 block over the geometric mip series.
///
/// # Panics
///
/// Panics if `size` is not a power of two; callers pass power-of-two map sizes.
pub fn bcn_chain_bytes(size: u32, block_bytes: u64) -> u64 {
    assert!(size.is_power_of_two(), "map size must be a power of two");
    let mut s = size;
    let mut total = 0u64;
    while s >= 4 {
        let blocks = (s / 4) as u64;
        total += blocks * blocks * block_bytes;
        s /= 2;
    }
    total
}

/// Exact cooked (block-payload) bytes of one tiled virtual texture: every fine tile carries its own
/// full sub-mip chain (down to 1×1 — the 2×2 and 1×1 levels pad to one 4×4 block each), plus the
/// resident tail (the coarse mips from `size >> tail_start` down to 4×4). This is what
/// [`cook_tiled_texture`] actually emits, so it is the number [`terrain_map_budget`] pins.
///
/// # Panics
///
/// Panics if `size`/`tile_size` are not powers of two with `tile_size` dividing `size`.
pub fn tiled_texture_bytes(size: u32, tile_size: u32, block_bytes: u64) -> u64 {
    assert!(size.is_power_of_two(), "map size must be a power of two");
    assert!(tile_size.is_power_of_two(), "tile size must be a power of two");
    assert!(
        size.is_multiple_of(tile_size),
        "tile size must divide the map size"
    );
    let tail_start = (size / tile_size).ilog2() + 1;
    // Number of tiles across all fine mips (each carries its own sub-mip chain).
    let mut tiles = 0u64;
    for mip in 0..tail_start {
        let per_side = (size / tile_size) >> mip;
        tiles += (per_side as u64) * (per_side as u64);
    }
    // Each tile is `tile_size` down to 1×1 (two extra padded blocks past the 4×4 floor).
    let tile_bytes = bcn_chain_bytes(tile_size, block_bytes) + 2 * block_bytes;
    // Tail: the coarse mips from `size >> tail_start` down to 4×4.
    let tail_bytes = bcn_chain_bytes(size >> tail_start, block_bytes);
    tiles * tile_bytes + tail_bytes
}

/// The canonical DO27 terrain-map texture set (one map).
#[derive(Clone, Copy, Debug)]
pub struct TerrainMapParams {
    /// Heightfield width in samples (e.g. 4096).
    pub heightfield_size: u32,
    /// Macro-map resolution in pixels (e.g. 8192).
    pub macro_size: u32,
    /// Number of RGBA splat masks (2).
    pub mask_count: u32,
    /// Number of material layers (6).
    pub layer_count: u32,
    /// Material-layer texture resolution in pixels (2048..4096).
    pub layer_size: u32,
    /// Number of maps in the package (DO27 targets 2).
    pub map_count: u32,
}

impl Default for TerrainMapParams {
    fn default() -> Self {
        Self {
            heightfield_size: 4096,
            macro_size: 8192,
            mask_count: 2,
            layer_count: 6,
            layer_size: 4096,
            map_count: 2,
        }
    }
}

/// Per-map cooked (on-disk) size and GPU residency, plus the per-profile budget headroom.
#[derive(Clone, Debug)]
pub struct TerrainMapBudget {
    /// Cooked bytes on disk for one map (all tiles + tail + heightfield, compressed).
    pub cooked_bytes: u64,
    /// Heightfield cooked bytes (R16, uncompressed — lz4 shaves this at pak time).
    pub heightfield_bytes: u64,
    /// Total cooked bytes for the whole package (`map_count` maps).
    pub total_cooked_bytes: u64,
    /// Resident GPU footprint if every tile of every map were loaded.
    pub gpu_full_bytes: u64,
    /// Per-profile budget and whether the full footprint fits.
    pub profiles: Vec<ProfileBudget>,
}

/// One profile's budget against the full footprint.
#[derive(Clone, Debug)]
pub struct ProfileBudget {
    /// Profile name.
    pub name: &'static str,
    /// Tile-cache budget, bytes.
    pub budget_bytes: u64,
    /// Whether the full footprint fits within the budget.
    pub fits: bool,
}

/// Bytes per 4×4 block of the one-channel slots (macro AO, layer height) on `platform`: BC4's 8
/// on desktop, ASTC 4×4's 16 on `"android"`, where every slot is ASTC 4×4 (ADR-5294). The other
/// slots are 16 bytes per block on both (BC7/BC5 or ASTC 4×4).
fn single_channel_block_bytes(platform: &str) -> u64 {
    if platform == "android" { 16 } else { 8 }
}

/// Computes the per-map cooked size and GPU footprint for a canonical map set, and checks it
/// against each profile's tile budget. Desktop block sizes; see
/// [`terrain_map_budget_for_platform`] for Android.
pub fn terrain_map_budget(params: &TerrainMapParams) -> TerrainMapBudget {
    terrain_map_budget_for_platform(params, "linux")
}

/// [`terrain_map_budget`] for a cook platform tag: `"android"` counts every slot at ASTC 4×4's
/// 16 bytes per block (AO and height included); any other tag counts the desktop BCn sizes
/// (BC4's 8 bytes per block for AO and height).
pub fn terrain_map_budget_for_platform(params: &TerrainMapParams, platform: &str) -> TerrainMapBudget {
    let macro_size = params.macro_size;
    let layer_size = params.layer_size;
    let tile_size = DEFAULT_TILE_SIZE;
    let single = single_channel_block_bytes(platform);

    // Macro maps: albedo (BC7), normal (BC5), AO (BC4). Each is a tiled virtual texture: every
    // page carries its own sub-mip chain, so the cooked size is `tiled_texture_bytes`, not the
    // plain full-map mip chain (`bcn_chain_bytes`).
    let albedo = tiled_texture_bytes(macro_size, tile_size, 16);
    let normal = tiled_texture_bytes(macro_size, tile_size, 16);
    let ao = tiled_texture_bytes(macro_size, tile_size, single);
    let masks = params.mask_count as u64 * tiled_texture_bytes(macro_size, tile_size, 16);
    // Material layers: albedo (BC7) + normal (BC5) + ORM (BC7) + height (BC4).
    let per_layer = tiled_texture_bytes(layer_size, tile_size, 16) * 3
        + tiled_texture_bytes(layer_size, tile_size, single);
    let layers = params.layer_count as u64 * per_layer;

    let heightfield_bytes = params.heightfield_size as u64 * params.heightfield_size as u64 * 2;
    let cooked_bytes = albedo + normal + ao + masks + layers + heightfield_bytes;
    // GPU footprint ≈ cooked bytes (BCn textures stay block-compressed in VRAM) + R16 heightfield.
    let total_cooked_bytes = cooked_bytes * params.map_count as u64;
    let gpu_full_bytes = total_cooked_bytes;

    let profiles = RENDER_PROFILES
        .iter()
        .map(|p| ProfileBudget {
            name: p.name,
            budget_bytes: p.tile_budget_bytes,
            fits: gpu_full_bytes <= p.tile_budget_bytes,
        })
        .collect();

    TerrainMapBudget {
        cooked_bytes,
        heightfield_bytes,
        total_cooked_bytes,
        gpu_full_bytes,
        profiles,
    }
}

/// A multi-line human-readable budget report (`regia cook --stats`), desktop block sizes.
pub fn format_budget_report(params: &TerrainMapParams, budget: &TerrainMapBudget) -> String {
    format_budget_report_for_platform(params, budget, "linux")
}

/// [`format_budget_report`] for a cook platform tag; pass the budget from
/// [`terrain_map_budget_for_platform`] with the same tag. `"android"` rows count every slot at
/// ASTC 4×4's 16 bytes per block.
pub fn format_budget_report_for_platform(
    params: &TerrainMapParams,
    budget: &TerrainMapBudget,
    platform: &str,
) -> String {
    let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let single = single_channel_block_bytes(platform);
    let mut out = String::new();
    if platform == "android" {
        out.push_str("terrain data budget (per map, android: ASTC 4x4 for every slot)\n");
    } else {
        out.push_str("terrain data budget (per map)\n");
    }
    out.push_str(&format!(
        "  heightfield {}x{} R16         {:>8.2} MiB\n",
        params.heightfield_size,
        params.heightfield_size,
        mib(budget.heightfield_bytes)
    ));
    out.push_str(&format!(
        "  8K macro albedo/normal/AO    {:>8.2} MiB\n",
        mib(tiled_texture_bytes(params.macro_size, DEFAULT_TILE_SIZE, 16) * 2
            + tiled_texture_bytes(params.macro_size, DEFAULT_TILE_SIZE, single))
    ));
    out.push_str(&format!(
        "  {}x 8K RGBA masks            {:>8.2} MiB\n",
        params.mask_count,
        mib(params.mask_count as u64 * tiled_texture_bytes(params.macro_size, DEFAULT_TILE_SIZE, 16))
    ));
    out.push_str(&format!(
        "  {} layers x 4 textures @ {}  {:>8.2} MiB\n",
        params.layer_count,
        params.layer_size,
        mib(params.layer_count as u64
            * (tiled_texture_bytes(params.layer_size, DEFAULT_TILE_SIZE, 16) * 3
                + tiled_texture_bytes(params.layer_size, DEFAULT_TILE_SIZE, single)))
    ));
    out.push_str(&format!(
        "  cooked total (1 map)         {:>8.2} GiB\n",
        gib(budget.cooked_bytes)
    ));
    out.push_str(&format!(
        "  cooked total ({} maps)       {:>8.2} GiB (target < 6 GiB)\n",
        params.map_count,
        gib(budget.total_cooked_bytes)
    ));
    out.push_str("  GPU residency:\n");
    for p in &budget.profiles {
        out.push_str(&format!(
            "    {:<22} budget {:>5.2} GiB, full footprint {:>5.2} GiB -> {}\n",
            p.name,
            gib(p.budget_bytes),
            gib(budget.gpu_full_bytes),
            if p.fits { "fits" } else { "streams" }
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::load_texture_bytes;

    /// A small deterministic 8-bit LCG; only used to synthesise test images.
    fn synth_rgba(size: u32, seed: u32) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::with_capacity((size * size * 4) as usize);
        for _ in 0..size * size {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let r = (state >> 24) as u8;
            let g = (state >> 16) as u8;
            let b = (state >> 8) as u8;
            let a = state as u8;
            out.extend_from_slice(&[r, g, b, a]);
        }
        out
    }

    fn synth_heightfield(size: u32, seed: u32) -> Heightfield {
        let mut state = seed | 1;
        let mut samples = Vec::with_capacity((size * size) as usize);
        for _ in 0..size * size {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            samples.push((state >> 16) as u16);
        }
        Heightfield {
            width: size,
            height: size,
            samples,
        }
    }

    /// An image whose every pixel's RGBA encodes its `(x, y)` position, so any tile/region
    /// misplacement produces a different colour and therefore different compressed bytes.
    fn position_rgba(size: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                out.extend_from_slice(&[x as u8, y as u8, ((x + y) & 0xff) as u8, 255]);
            }
        }
        out
    }

    /// Independent reference extraction of a `tile`×`tile` region at tile `(tx, ty)` from a
    /// `size`×`size` RGBA8 image. Deliberately written as a plain pixel loop (not [`extract_tile`])
    /// so a bug in the cooker's extraction is caught by the coverage test.
    fn reference_region(src: &[u8], size: u32, tile: u32, tx: u32, ty: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((tile * tile * 4) as usize);
        for sy in (ty * tile)..(ty * tile + tile) {
            for sx in (tx * tile)..(tx * tile + tile) {
                let i = ((sy * size + sx) * 4) as usize;
                out.extend_from_slice(&src[i..i + 4]);
            }
        }
        out
    }

    /// Cooks one standalone `tile`×`tile` region through the same codec path `cook_tiled_texture`
    /// uses for a tile, so the coverage test can compare tile bytes against the correct region.
    fn cook_region_standalone(tile: u32, region: &[u8], format: TileFormat) -> Vec<u8> {
        cook_ktx2_from_mips(
            tile,
            tile,
            &generate_mips_rgba8(tile, tile, region),
            format.usage(),
            "linux",
            TextureCookQuality::High,
        )
        .expect("region cook")
    }

    #[test]
    fn tiled_texture_covers_every_pixel_exactly_once() {
        // 64x64 source, 16x16 tiles -> mips 64,32,16 tiled; 8,4 tail.
        let size = 64u32;
        let tile = 16u32;
        let cooked = cook_tiled_texture(
            "test",
            size,
            size,
            &synth_rgba(size, 7),
            tile,
            TileFormat::Bc7Srgb,
            "linux",
            TextureCookQuality::High,
        )
        .expect("cook");

        assert_eq!(cooked.mip_count, 7); // 64,32,16,8,4,2,1
        // Tiled mips: 64 (4x4 tiles), 32 (2x2), 16 (1x1).
        assert_eq!(cooked.tiles.len(), 16 + 4 + 1);
        // Tail mips: 8 and 4 (2 and 1 are below the block-compressible minimum).
        assert_eq!(cooked.tail_mips, 2);
        for t in &cooked.tiles {
            let reader = ktx2::Reader::new(&t.ktx2).expect("tile ktx2");
            assert_eq!(reader.header().pixel_width, tile);
            // Record the block format: albedo tiles are BC7 sRGB.
            assert_eq!(reader.header().format, Some(ktx2::Format::BC7_SRGB_BLOCK));
            // Full sub-mip chain: 16,8,4,2,1.
            assert_eq!(reader.levels().count(), 5);
        }
        // Tail = one KTX2 with the 8x8 and 4x4 mips.
        let tail_reader = ktx2::Reader::new(&cooked.tail_ktx2).expect("tail ktx2");
        assert_eq!(tail_reader.header().pixel_width, 8);
        assert_eq!(tail_reader.header().format, Some(ktx2::Format::BC7_SRGB_BLOCK));
        assert_eq!(tail_reader.levels().count(), 2);
    }

    #[test]
    fn tiled_texture_maps_each_tile_to_its_source_region() {
        // 32x32 source, 16x16 tiles -> 2x2 tiles at mip 0. Every pixel encodes its position, so a
        // tile cooked from the wrong region produces different bytes than the correct region.
        let size = 32u32;
        let tile = 16u32;
        let src = position_rgba(size);
        let cooked = cook_tiled_texture(
            "test",
            size,
            size,
            &src,
            tile,
            TileFormat::Bc7Srgb,
            "linux",
            TextureCookQuality::High,
        )
        .expect("cook");

        for (tx, ty) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
            let region = reference_region(&src, size, tile, tx, ty);
            let expected = cook_region_standalone(tile, &region, TileFormat::Bc7Srgb);
            let actual = cooked
                .tiles
                .iter()
                .find(|t| t.mip == 0 && t.x == tx && t.y == ty)
                .expect("mip-0 tile present");
            assert_eq!(
                actual.ktx2, expected,
                "mip-0 tile ({tx},{ty}) does not match its source region"
            );
        }
    }

    #[test]
    fn tiled_texture_is_deterministic() {
        let size = 32u32;
        let tile = 8u32;
        let a = cook_tiled_texture(
            "d",
            size,
            size,
            &synth_rgba(size, 99),
            tile,
            TileFormat::Bc7Linear,
            "linux",
            TextureCookQuality::High,
        )
        .expect("cook a");
        let b = cook_tiled_texture(
            "d",
            size,
            size,
            &synth_rgba(size, 99),
            tile,
            TileFormat::Bc7Linear,
            "linux",
            TextureCookQuality::High,
        )
        .expect("cook b");
        assert_eq!(a, b, "same input must cook to the same bytes");
    }

    #[test]
    fn tiled_texture_rejects_bad_inputs() {
        assert!(matches!(
            cook_tiled_texture(
                "x",
                32,
                16,
                &synth_rgba(32, 1),
                8,
                TileFormat::Bc7Srgb,
                "linux",
                TextureCookQuality::High,
            ),
            Err(TerrainDataError::NotSquare { .. })
        ));
        assert!(matches!(
            cook_tiled_texture(
                "x",
                32,
                32,
                &synth_rgba(32, 1),
                6,
                TileFormat::Bc7Srgb,
                "linux",
                TextureCookQuality::High,
            ),
            Err(TerrainDataError::BadTileSize { .. })
        ));
        // 24 is a multiple of the tile size 8 but not a power of two: its mips would leave
        // texels in no tile and not in the tail.
        assert!(matches!(
            cook_tiled_texture(
                "x",
                24,
                24,
                &synth_rgba(24, 1),
                8,
                TileFormat::Bc7Srgb,
                "linux",
                TextureCookQuality::High,
            ),
            Err(TerrainDataError::BadTileSize { .. })
        ));
        // A 4×4 map tiled at 2 is a legal desktop grid (power of two, tile divides the map)
        // and an illegal ASTC grid. The refusal happens before the buffer is read and before
        // any encode: an empty buffer would be `BadBuffer` if encoding had started.
        let aligned = cook_tiled_texture(
            "x",
            4,
            4,
            &[],
            2,
            TileFormat::Bc4,
            "android",
            TextureCookQuality::High,
        );
        match aligned {
            Err(
                err @ TerrainDataError::AstcAlignment {
                    tile_size: 2,
                    block: 4,
                },
            ) => {
                assert!(err.to_string().contains("terrain_astc_alignment"), "{err}");
            }
            other => panic!("expected terrain_astc_alignment, got {other:?}"),
        }
    }

    /// BLAKE3 of the desktop BCn KTX2 bytes (tail, then tiles in cook order) for the fixture in
    /// [`desktop_bcn_bytes_stay_put`], covering all four desktop slot formats in test order.
    /// Computed with the pre-ASTC cooker at `a0cf3786c`; an Android change cannot move this path.
    const DESKTOP_BCN_FIXTURE_BLAKE3: &str =
        "0bd02fe5cc8481c643d704820e16ac89b6320860fe99de4b4fef1a7feb21752b";

    fn constant_rgba(size: u32, texel: [u8; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity((size * size * 4) as usize);
        for _ in 0..size * size {
            out.extend_from_slice(&texel);
        }
        out
    }

    fn concat_cooked(cooked: &TiledTextureCook) -> Vec<u8> {
        let mut bytes = cooked.tail_ktx2.clone();
        for tile in &cooked.tiles {
            bytes.extend_from_slice(&tile.ktx2);
        }
        bytes
    }

    #[test]
    fn desktop_bcn_bytes_stay_put() {
        let size = 16u32;
        let mut hash = blake3::Hasher::new();
        for format in [
            TileFormat::Bc7Linear,
            TileFormat::Bc7Srgb,
            TileFormat::Bc4,
            TileFormat::Bc5,
        ] {
            let cooked = cook_tiled_texture(
                "lock",
                size,
                size,
                &constant_rgba(size, [10, 20, 30, 255]),
                size,
                format,
                "linux",
                TextureCookQuality::High,
            )
            .expect("desktop cook");
            hash.update(&concat_cooked(&cooked));
        }
        let hex = hash.finalize().to_hex();
        assert_eq!(
            hex.as_str(),
            DESKTOP_BCN_FIXTURE_BLAKE3,
            "desktop BCn fixture differs from the pre-ASTC baseline: {hex}"
        );
    }

    /// [`desktop_noise_tiles_stay_put`]: BLAKE3 of the four desktop slot formats of a 256² noise
    /// map tiled at 32 (85 tiles, so the threaded tile encoder runs on any multi-core host), in
    /// test order. Computed at `03e71f2de` (the reviewed ADR-5294 encoder, before the lazy-tile
    /// change) on a Zen 2 (AVX2) host. The same after the change on AVX-512 hosts, on an Apple
    /// M5 (NEON) and under emulated SSE2-only, SSE4.2 and AVX-only CPUs: every kernel target the
    /// BCn encoder dispatches to (ADR-5799).
    const DESKTOP_NOISE_TILES_BLAKE3: &str =
        "deebb831a8fa4681c1b8cbd6b547a3622d8b252f078a342da9efe502932ca26a";

    /// Pins the desktop tile bytes of real (noise) content through the threaded tile encoder;
    /// [`desktop_bcn_bytes_stay_put`] pins one constant tile, which takes the serial branch.
    #[test]
    fn desktop_noise_tiles_stay_put() {
        let (size, tile) = (256u32, 32u32);
        let rgba = synth_rgba(size, 0x5eed);
        let mut hash = blake3::Hasher::new();
        for format in [
            TileFormat::Bc7Linear,
            TileFormat::Bc7Srgb,
            TileFormat::Bc4,
            TileFormat::Bc5,
        ] {
            let cooked = cook_tiled_texture(
                "noise",
                size,
                size,
                &rgba,
                tile,
                format,
                "linux",
                TextureCookQuality::High,
            )
            .expect("desktop cook");
            assert_eq!(cooked.tiles.len(), 64 + 16 + 4 + 1);
            hash.update(&concat_cooked(&cooked));
        }
        let hex = hash.finalize().to_hex();
        assert_eq!(
            hex.as_str(),
            DESKTOP_NOISE_TILES_BLAKE3,
            "desktop noise tiles differ from the ADR-5294 baseline: {hex}"
        );
    }

    /// [`android_astc_tiles_are_the_same_on_every_host`]: BLAKE3 of the four Android slot formats
    /// of a 64² noise map tiled at 32 (4 + 1 ASTC 4×4 tiles and the tail per format), in test
    /// order. Measured on x86_64 (the build server, AVX2 variant) and required unchanged on aarch64
    /// (Apple M5, NEON) and on every x86_64 variant astcenc dispatches to (ADR-7878). Never
    /// re-pin to make one host pass.
    const ANDROID_ASTC_TILES_BLAKE3: &str =
        "e96a67a7d1aa7b3bcadb79f001961cfb68b4abcdbcda6124dd3dac148c0f4a70";

    /// Android terrain tiles are the same bytes on every cook host: astcenc is built invariant
    /// (no `ASTCENC_NO_INVARIANCE`, no floating-point contraction; `.cargo/config.toml` `[env]`),
    /// so its SSE2, SSE4.1, AVX2 and NEON builds agree.
    #[test]
    fn android_astc_tiles_are_the_same_on_every_host() {
        let (size, tile) = (64u32, 32u32);
        let rgba = synth_rgba(size, 0x0a57c);
        let mut hash = blake3::Hasher::new();
        for format in [
            TileFormat::Bc7Linear,
            TileFormat::Bc7Srgb,
            TileFormat::Bc4,
            TileFormat::Bc5,
        ] {
            let cooked = cook_tiled_texture(
                "noise",
                size,
                size,
                &rgba,
                tile,
                format,
                "android",
                TextureCookQuality::High,
            )
            .expect("android cook");
            assert_eq!(cooked.tiles.len(), 4 + 1);
            hash.update(&concat_cooked(&cooked));
        }
        let hex = hash.finalize().to_hex();
        eprintln!("android astc tiles {}: blake3 {hex}", std::env::consts::ARCH);
        assert_eq!(
            hex.as_str(),
            ANDROID_ASTC_TILES_BLAKE3,
            "the Android ASTC tiles cook to other bytes on this host ({})",
            std::env::consts::ARCH
        );
    }

    #[test]
    fn astc_encoder_reject_is_terrain_astc_encode() {
        // 4×4 with a short buffer is a size the encoder refuses (`BadBuffer`). The tiled cook
        // rejects that shape earlier; this pins the mapping used when the encoder does refuse.
        let err =
            cook_chain_astc(4, 4, &[(4, 4, vec![1, 2, 3])], TileFormat::Bc7Linear).expect_err("short buffer");
        assert!(matches!(err, TerrainDataError::AstcEncode(_)), "{err:?}");
        assert!(err.to_string().contains("terrain_astc_encode"), "{err}");
    }

    fn assert_astc_tile(ktx2: &[u8], edge: u32, srgb: bool, texel: [u8; 4]) {
        // KTX 2.0 § 3.1 identifier, then vkFormat at byte 12 (little-endian).
        const KTX2_IDENTIFIER: [u8; 12] = [
            0xAB, 0x4B, 0x54, 0x58, 0x20, 0x32, 0x30, 0xBB, 0x0D, 0x0A, 0x1A, 0x0A,
        ];
        assert_eq!(&ktx2[..12], &KTX2_IDENTIFIER, "KTX2 identifier");
        let vk = u32::from_le_bytes(ktx2[12..16].try_into().expect("vkFormat"));
        let expect_vk = if srgb { 158 } else { 157 };
        assert_eq!(vk, expect_vk, "ASTC 4x4 vkFormat");
        let reader = ktx2::Reader::new(ktx2).expect("ktx2");
        assert_eq!(reader.header().pixel_width, edge);
        assert_eq!(reader.header().pixel_height, edge);
        let expect_format = if srgb {
            ktx2::Format::ASTC_4x4_SRGB_BLOCK
        } else {
            ktx2::Format::ASTC_4x4_UNORM_BLOCK
        };
        assert_eq!(reader.header().format, Some(expect_format));
        // Full chain down to 1×1. 2×2 and 1×1 each pad to one 16-byte block.
        let mut blocks = 0usize;
        let mut level0 = Vec::new();
        for (i, level) in reader.levels().enumerate() {
            assert!(
                level.data.len().is_multiple_of(16),
                "level {i} is not whole blocks"
            );
            blocks += level.data.len() / 16;
            let mip_edge = (edge >> i).max(1);
            let pixels = regia_asset::astc_decode::decode_astc(mip_edge, mip_edge, (4, 4), srgb, level.data)
                .expect("runtime decode mip, including padded 2x2 and 1x1");
            assert_eq!(pixels.len(), (mip_edge * mip_edge * 4) as usize);
            assert!(pixels.chunks(4).all(|pixel| pixel == texel), "mip {i} color");
            if i == 0 {
                let side = edge / ASTC_BLOCK_EDGE;
                assert_eq!(
                    level.data.len(),
                    (side * side) as usize * 16,
                    "level 0 block count"
                );
                level0 = level.data.to_vec();
            }
        }
        let levels = edge.ilog2() + 1;
        // Whole blocks from `edge` down to 4, plus one block each for 2 and 1.
        let mut expected_blocks = 0usize;
        let mut s = edge;
        while s >= 4 {
            let n = (s / 4) as usize;
            expected_blocks += n * n;
            s /= 2;
        }
        expected_blocks += 2;
        assert_eq!(blocks, expected_blocks, "chain block count");
        assert_eq!(reader.levels().count(), levels as usize);
        let decoded =
            crate::astc_encode::decode_image(edge, edge, &level0, crate::astc_encode::BlockSize::Block4x4)
                .expect("decode level 0");
        assert!(
            decoded.chunks(4).all(|px| px == texel),
            "constant color did not round-trip"
        );
        let runtime_decoded = regia_asset::astc_decode::decode_astc(edge, edge, (4, 4), srgb, &level0)
            .expect("runtime ASTC decoder");
        assert_eq!(runtime_decoded, decoded, "runtime and encoder decoders agree");
        let mut block = [0u8; 16];
        block.copy_from_slice(&level0[..16]);
        let info = crate::astc_encode::inspect_block(&block, crate::astc_encode::BlockSize::Block4x4);
        assert_eq!(info.kind, crate::astc_encode::BlockKind::VoidExtent);
        assert_eq!(info.constant_color, Some(texel));
    }

    #[test]
    fn android_constant_tiles_are_astc_4x4() {
        let size = 16u32;
        let texel = [10u8, 20, 30, 255];
        let rgba = constant_rgba(size, texel);
        for (format, srgb) in [
            (TileFormat::Bc7Linear, false),
            (TileFormat::Bc7Srgb, true),
            (TileFormat::Bc4, false),
            (TileFormat::Bc5, false),
        ] {
            let cooked = cook_tiled_texture(
                "slot",
                size,
                size,
                &rgba,
                size,
                format,
                "android",
                TextureCookQuality::High,
            )
            .expect("android cook");
            assert_eq!(cooked.tiles.len(), 1, "{format:?}");
            assert_eq!(cooked.tail_mips, 2, "{format:?}");
            // The slot's channel layout, as the Android terrain arrays store it: AO / height in
            // R = G = B, normals in `.rg` like BC5 (`R G 0 1`); the desktop BCn decoder likewise
            // returns only the channels BC4 / BC5 store.
            let stored = match format {
                TileFormat::Bc4 => [texel[0], texel[0], texel[0], 255],
                TileFormat::Bc5 => [texel[0], texel[1], 0, 255],
                TileFormat::Bc7Srgb | TileFormat::Bc7Linear => texel,
            };
            assert_astc_tile(&cooked.tiles[0].ktx2, size, srgb, stored);
            let tail = ktx2::Reader::new(&cooked.tail_ktx2).expect("tail");
            let expect = if srgb {
                ktx2::Format::ASTC_4x4_SRGB_BLOCK
            } else {
                ktx2::Format::ASTC_4x4_UNORM_BLOCK
            };
            assert_eq!(tail.header().format, Some(expect), "{format:?} tail");
            assert_eq!(tail.header().pixel_width, 8);
            assert_eq!(tail.levels().count(), 2);
        }
    }

    #[test]
    fn android_low_quality_keeps_four_texel_blocks() {
        let texel = [40, 50, 60, 255];
        let cooked = cook_tiled_texture(
            "mask",
            16,
            16,
            &constant_rgba(16, texel),
            16,
            TileFormat::Bc7Linear,
            "android",
            TextureCookQuality::Low,
        )
        .expect("low-quality tiled cook");
        assert_astc_tile(&cooked.tiles[0].ktx2, 16, false, texel);
    }

    #[test]
    fn android_misaligned_non_power_of_two_tiles_fail_before_buffer_validation() {
        for tile_size in [3, 6, 10] {
            let err = cook_tiled_texture(
                "mask",
                16,
                16,
                &[],
                tile_size,
                TileFormat::Bc7Linear,
                "android",
                TextureCookQuality::High,
            )
            .expect_err("misaligned tile");
            assert!(matches!(err, TerrainDataError::AstcAlignment { .. }));
        }
    }

    #[test]
    fn android_tiles_decode_to_their_source_regions() {
        let colors = [
            [20, 30, 40, 255],
            [50, 60, 70, 255],
            [80, 90, 100, 255],
            [110, 120, 130, 255],
        ];
        let mut rgba = Vec::new();
        for y in 0..16 {
            for x in 0..16 {
                rgba.extend_from_slice(&colors[(y / 8) * 2 + x / 8]);
            }
        }
        let cooked = cook_tiled_texture(
            "mask",
            16,
            16,
            &rgba,
            8,
            TileFormat::Bc7Linear,
            "android",
            TextureCookQuality::High,
        )
        .expect("tiled cook");
        assert_eq!(cooked.tiles.len(), 5); // Four fine tiles, one tile at mip 1.
        for (i, tile) in cooked.tiles.iter().take(4).enumerate() {
            assert_eq!((tile.mip, tile.x, tile.y), (0, i as u32 % 2, i as u32 / 2));
            assert_astc_tile(&tile.ktx2, 8, false, colors[i]);
        }
        let tail = ktx2::Reader::new(&cooked.tail_ktx2).expect("tail");
        assert_eq!(tail.header().pixel_width, 4);
        assert_eq!(tail.levels().count(), 1);
        assert_eq!(tail.levels().next().expect("tail level").data.len(), 16);
    }

    #[test]
    fn android_tiles_ignore_bcn_quality_ladder() {
        let size = 16u32;
        let rgba = constant_rgba(size, [4, 5, 6, 255]);
        let a = cook_tiled_texture(
            "d",
            size,
            size,
            &rgba,
            size,
            TileFormat::Bc4,
            "android",
            TextureCookQuality::Low,
        )
        .expect("a");
        let b = cook_tiled_texture(
            "d",
            size,
            size,
            &rgba,
            size,
            TileFormat::Bc4,
            "android",
            TextureCookQuality::High,
        )
        .expect("b");
        assert_eq!(
            a, b,
            "the tiled ASTC path ignores the BCn quality ladder and stays 4x4"
        );
    }

    #[test]
    fn heightfield_tiles_keep_16bit_and_compute_minmax() {
        let hf = synth_heightfield(64, 5);
        let tiled = cook_tiled_heightfield(&hf, 16).expect("cook");
        assert_eq!(tiled.version, TILED_HEIGHTFIELD_FORMAT_VERSION);
        assert_eq!(tiled.tiles.len(), 16); // 4x4 tiles
        // Every sample is preserved exactly at its source position and per-tile min/max bracket it.
        for tile in &tiled.tiles {
            assert_eq!(tile.samples.len(), 16 * 16);
            let mn = tile.samples.iter().copied().min().expect("non-empty");
            let mx = tile.samples.iter().copied().max().expect("non-empty");
            assert_eq!((tile.min, tile.max), (mn, mx));
            for row in 0..16 {
                for col in 0..16 {
                    let src_row = tile.y * 16 + row;
                    let src_col = tile.x * 16 + col;
                    let expected = hf.samples[(src_row * 64 + src_col) as usize];
                    assert_eq!(tile.samples[(row * 16 + col) as usize], expected);
                }
            }
        }
    }

    #[test]
    fn heightfield_tiles_pad_non_multiple_edge() {
        let hf = synth_heightfield(40, 5); // 40 not a multiple of 16 -> 3x3 tiles with padding
        let tiled = cook_tiled_heightfield(&hf, 16).expect("cook");
        assert_eq!(tiled.tiles.len(), 9);
        let (w, h) = (hf.width, hf.height);
        for tile in &tiled.tiles {
            assert_eq!(tile.samples.len(), 256);
            // Padded samples (past the map edge) must repeat the border sample, and in-bounds
            // samples must match their source exactly — both come from the same clamp.
            for row in 0..16 {
                for col in 0..16 {
                    let src_col = (tile.x * 16 + col).min(w - 1);
                    let src_row = (tile.y * 16 + row).min(h - 1);
                    let expected = hf.samples[(src_row * w + src_col) as usize];
                    assert_eq!(
                        tile.samples[(row * 16 + col) as usize],
                        expected,
                        "tile ({},{}) sample ({row},{col}) must repeat the border sample",
                        tile.x,
                        tile.y
                    );
                }
            }
        }
    }

    #[test]
    fn tile_asset_id_is_stable_and_distinct() {
        let map = AssetId::sub_asset(AssetId::NIL, "test-map");
        let a = tile_asset_id(map, "macro_albedo", 2, 3, 4);
        let b = tile_asset_id(map, "macro_albedo", 2, 3, 4);
        let c = tile_asset_id(map, "macro_albedo", 2, 3, 5);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            tile_asset_path("macro_albedo", 2, 3, 4),
            "terrain/macro_albedo/mip2/tile_3_4"
        );
    }

    #[test]
    fn budget_is_within_package_target() {
        let params = TerrainMapParams::default();
        let budget = terrain_map_budget(&params);
        // The exact per-map cooked size is pinned so a format/arithmetic change is caught. This
        // includes each page's own sub-mip chain plus the tail (~1.32x the plain full-map chain).
        assert_eq!(budget.cooked_bytes, 1_196_943_680);
        // The two-map package must fit well under the 6 GiB target.
        assert!(
            budget.total_cooked_bytes < 6 * 1024 * 1024 * 1024,
            "package exceeds 6 GiB: {} bytes",
            budget.total_cooked_bytes
        );
        // The full footprint fits the desktop profiles and streams on Deck Medium.
        assert!(budget.profiles[0].fits, "5090 must hold the full footprint");
        assert!(budget.profiles[1].fits, "7900 XTX must hold the full footprint");
        assert!(
            !budget.profiles[2].fits,
            "Deck Medium must stream (footprint > 1 GiB)"
        );
        let report = format_budget_report(&params, &budget);
        assert!(report.contains("target < 6 GiB"));
    }

    /// Finding 5 (fixrev-terrain-tiles-g): the Android budget counts AO and height at ASTC
    /// 4×4's 16 bytes per block, like every other Android slot; desktop keeps BC4's 8.
    #[test]
    fn android_budget_counts_every_slot_at_sixteen_bytes_per_block() {
        let params = TerrainMapParams::default();
        let desktop = terrain_map_budget(&params);
        assert_eq!(
            terrain_map_budget_for_platform(&params, "linux").cooked_bytes,
            desktop.cooked_bytes
        );
        let android = terrain_map_budget_for_platform(&params, "android");
        // AO (one macro slot) and height (one per layer) move from 8 to 16 bytes per block.
        let extra = tiled_texture_bytes(params.macro_size, DEFAULT_TILE_SIZE, 8)
            + params.layer_count as u64 * tiled_texture_bytes(params.layer_size, DEFAULT_TILE_SIZE, 8);
        assert_eq!(android.cooked_bytes, desktop.cooked_bytes + extra);
        assert_eq!(
            android.total_cooked_bytes,
            android.cooked_bytes * params.map_count as u64
        );
        // Every Android texture slot is 16 bytes per block: albedo, normal, AO and the masks,
        // and four slots per layer.
        let all16 = (3 + params.mask_count as u64)
            * tiled_texture_bytes(params.macro_size, DEFAULT_TILE_SIZE, 16)
            + 4 * params.layer_count as u64 * tiled_texture_bytes(params.layer_size, DEFAULT_TILE_SIZE, 16);
        assert_eq!(android.cooked_bytes, all16 + android.heightfield_bytes);
        let report = format_budget_report_for_platform(&params, &android, "android");
        assert!(
            report.starts_with("terrain data budget (per map, android: ASTC 4x4"),
            "{report}"
        );
        assert_eq!(
            format_budget_report(&params, &desktop),
            format_budget_report_for_platform(&params, &desktop, "linux")
        );
    }

    #[test]
    fn bcn_chain_bytes_matches_geometric_sum() {
        // 4x4 -> exactly one block.
        assert_eq!(bcn_chain_bytes(4, 16), 16);
        assert_eq!(bcn_chain_bytes(4, 8), 8);
        // 8x8 -> 4 blocks (mip 8) + 1 block (mip 4).
        assert_eq!(bcn_chain_bytes(8, 16), 4 * 16 + 16);
    }

    #[test]
    fn tiled_texture_bytes_matches_the_tiled_layout() {
        // 64x64 tiled at 16: fine mips 64 (4x4), 32 (2x2), 16 (1x1) = 16+4+1 = 21 tiles; each tile
        // is bcn_chain_bytes(16, 16)=16+4+1=21 blocks (16,8,4) + 2 padded blocks (2,1) = 23 blocks.
        let tile_bytes = bcn_chain_bytes(16, 16) + 2 * 16;
        assert_eq!(tile_bytes, 23 * 16);
        // Tail: mips 8,4 -> (2² + 1²) blocks = 5 blocks.
        let tail_bytes = bcn_chain_bytes(8, 16);
        assert_eq!(tail_bytes, 5 * 16);
        assert_eq!(tiled_texture_bytes(64, 16, 16), 21 * tile_bytes + tail_bytes);
    }

    #[test]
    fn write_tiled_texture_cooked_round_trips_through_load_texture_bytes() {
        // 32x32 tiled at 16 -> 2x2 mip-0 tiles + 1x1 mip-1 tile + tail (8,4).
        let size = 32u32;
        let tile = 16u32;
        let cooked = cook_tiled_texture(
            "macro_albedo",
            size,
            size,
            &synth_rgba(size, 11),
            tile,
            TileFormat::Bc7Srgb,
            "linux",
            TextureCookQuality::High,
        )
        .expect("cook");
        let map = AssetId::sub_asset(AssetId::NIL, "test-map");
        let dir = std::env::temp_dir().join(format!("regia-terrain-cook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let written = write_tiled_texture_cooked(map, &cooked, &dir).expect("write");
        // Tail + (4 mip-0 tiles + 1 mip-1 tile) = 6 files.
        assert_eq!(written.len(), 1 + cooked.tiles.len());

        // The tail round-trips at its deterministic id.
        let tail_id = tail_asset_id(map, "macro_albedo");
        let (header, tex) =
            load_texture_bytes(&std::fs::read(dir.join(cooked_file_name(tail_id))).expect("read"))
                .expect("decode tail");
        assert_eq!(header.asset_id, tail_id);
        assert_eq!(tex.ktx2, cooked.tail_ktx2);

        // Every tile round-trips at its deterministic id.
        for t in &cooked.tiles {
            let id = tile_asset_id(map, "macro_albedo", t.mip, t.x, t.y);
            let bytes = std::fs::read(dir.join(cooked_file_name(id))).expect("read");
            let (header, tex) = load_texture_bytes(&bytes).expect("decode tile");
            assert_eq!(header.asset_id, id);
            assert_eq!(tex.ktx2, t.ktx2);
            assert_eq!(tex.width, tile);
            assert_eq!(tex.height, tile);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_asset_id_is_stable_and_distinct_from_tiles() {
        let map = AssetId::sub_asset(AssetId::NIL, "test-map");
        assert_eq!(tail_asset_path("macro_albedo"), "terrain/macro_albedo/tail");
        assert_eq!(
            tail_asset_id(map, "macro_albedo"),
            tail_asset_id(map, "macro_albedo")
        );
        assert_ne!(
            tail_asset_id(map, "macro_albedo"),
            tile_asset_id(map, "macro_albedo", 0, 0, 0)
        );
    }

    // -----------------------------------------------------------------------
    // Manual benches (`--ignored`): run once with `--release -- --ignored --nocapture`.
    // Numbers are recorded in `docs/evidence/terrain-data/budgets.md`.
    // -----------------------------------------------------------------------

    /// Peak resident set size (high-water mark) in bytes. Linux reads `/proc/self/status`; other
    /// platforms report `0` (these benches are manual, Linux-only runs).
    fn peak_rss_bytes() -> u64 {
        #[cfg(target_os = "linux")]
        {
            let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmHWM:") {
                    let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().unwrap_or(0);
                    return kb * 1024;
                }
            }
            0
        }
        #[cfg(not(target_os = "linux"))]
        {
            0
        }
    }

    /// Manual bench: cook a full 4096² R16 heightfield into 256-sample tiles with per-tile min/max.
    #[test]
    #[ignore = "manual bench: a full 4096² heightfield cook"]
    fn bench_cook_heightfield_4096() {
        let size = 4096u32;
        let hf = synth_heightfield(size, 42);
        let start = std::time::Instant::now();
        let tiled = cook_tiled_heightfield(&hf, DEFAULT_TILE_SIZE).expect("cook");
        let elapsed = start.elapsed();
        let bytes: usize = tiled.tiles.iter().map(|t| t.samples.len() * 2).sum();
        assert_eq!(tiled.tiles.len(), 256);
        assert_eq!(bytes, 4096 * 4096 * 2);
        eprintln!(
            "bench heightfield 4096²: {} tiles, {bytes} sample bytes, {elapsed:?}, peak RSS {} MiB",
            tiled.tiles.len(),
            peak_rss_bytes() / 1024 / 1024
        );
    }

    /// Manual bench: cook an 8K macro mask (BC7 linear) into 256-pixel tiles + resident tail.
    #[test]
    #[ignore = "manual bench: an 8K macro mask cook"]
    fn bench_cook_mask_8k() {
        let size = 8192u32;
        let rgba = synth_rgba(size, 7);
        let start = std::time::Instant::now();
        let cooked = cook_tiled_texture(
            "mask0",
            size,
            size,
            &rgba,
            DEFAULT_TILE_SIZE,
            TileFormat::Bc7Linear,
            "linux",
            TextureCookQuality::High,
        )
        .expect("cook");
        let elapsed = start.elapsed();
        let bytes: usize = cooked.tail_ktx2.len() + cooked.tiles.iter().map(|t| t.ktx2.len()).sum::<usize>();
        assert_eq!(cooked.tiles.len(), 1365); // 32²+16²+8²+4²+2²+1²
        eprintln!(
            "bench 8K mask (BC7): {} tiles, {bytes} cooked bytes, {elapsed:?}, peak RSS {} MiB",
            cooked.tiles.len(),
            peak_rss_bytes() / 1024 / 1024
        );
    }

    /// Smooth multi-octave value noise in `0..=255` (deterministic integer lattice, `f32`
    /// arithmetic only): one natural-looking channel with detail down to 4-texel cells.
    fn smooth_field(size: u32, seed: u32) -> Vec<f32> {
        let lattice = |gx: u32, gy: u32, octave: u32| -> f32 {
            let mut h = gx.wrapping_mul(0x9E37_79B1)
                ^ gy.wrapping_mul(0x85EB_CA77)
                ^ seed.wrapping_mul(0xC2B2_AE3D)
                ^ octave.wrapping_mul(0x27D4_EB2F);
            h ^= h >> 15;
            h = h.wrapping_mul(0x2C1B_3C6D);
            h ^= h >> 12;
            (h & 0xFFFF) as f32 / 65_535.0
        };
        let mut out = Vec::with_capacity((size * size) as usize);
        for y in 0..size {
            for x in 0..size {
                let (mut v, mut amp, mut total) = (0.0f32, 1.0f32, 0.0f32);
                for octave in 0..7u32 {
                    let cell = (size >> (octave + 1)).max(2) as f32;
                    let (fx, fy) = (x as f32 / cell, y as f32 / cell);
                    let (gx, gy) = (fx as u32, fy as u32);
                    let (tx, ty) = (fx - gx as f32, fy - gy as f32);
                    let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
                    let top = lattice(gx, gy, octave) * (1.0 - sx) + lattice(gx + 1, gy, octave) * sx;
                    let bottom =
                        lattice(gx, gy + 1, octave) * (1.0 - sx) + lattice(gx + 1, gy + 1, octave) * sx;
                    v += amp * (top * (1.0 - sy) + bottom * sy);
                    total += amp;
                    amp *= 0.6;
                }
                out.push(v / total * 255.0);
            }
        }
        out
    }

    /// The RGBA8 source a terrain cook would read for `format` (§ 9 inputs): albedo RGB, linear
    /// RGBA masks, a tangent normal built from a height field, or a gray height (L replicated).
    fn terrain_like_rgba(size: u32, format: TileFormat) -> Vec<u8> {
        let byte = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        let fields: Vec<Vec<f32>> = (0..4).map(|c| smooth_field(size, 11 + c)).collect();
        let mut out = Vec::with_capacity((size * size * 4) as usize);
        for i in 0..(size * size) as usize {
            let texel = match format {
                TileFormat::Bc7Srgb => [
                    byte(fields[0][i] * 0.45 + 20.0),
                    byte(fields[1][i] * 0.5 + 30.0),
                    byte(fields[2][i] * 0.3 + 10.0),
                    255,
                ],
                TileFormat::Bc7Linear => [
                    byte(fields[0][i]),
                    byte(fields[1][i]),
                    byte(fields[2][i]),
                    byte(fields[3][i]),
                ],
                TileFormat::Bc5 => {
                    let (x, y) = (i as u32 % size, i as u32 / size);
                    let at =
                        |xx: u32, yy: u32| fields[0][(yy.min(size - 1) * size + xx.min(size - 1)) as usize];
                    let dx = (at(x + 1, y) - at(x.saturating_sub(1), y)) * 0.02;
                    let dy = (at(x, y + 1) - at(x, y.saturating_sub(1))) * 0.02;
                    let len = (dx * dx + dy * dy + 1.0).sqrt();
                    [
                        byte((-dx / len * 0.5 + 0.5) * 255.0),
                        byte((-dy / len * 0.5 + 0.5) * 255.0),
                        byte((1.0 / len * 0.5 + 0.5) * 255.0),
                        255,
                    ]
                }
                TileFormat::Bc4 => {
                    let v = byte(fields[0][i]);
                    [v, v, v, 255]
                }
            };
            out.extend_from_slice(&texel);
        }
        out
    }

    /// Mip 0 of a tiled cook, decoded with the engine's CPU reference decoders (`bcn_decode`,
    /// `astc_decode`) and stitched back to `size`×`size` RGBA8.
    fn decode_mip0(cooked: &TiledTextureCook, platform: &str) -> Vec<u8> {
        let (size, tile) = (cooked.width as usize, cooked.tile_size as usize);
        let mut out = vec![0u8; size * size * 4];
        for t in cooked.tiles.iter().filter(|t| t.mip == 0) {
            let reader = ktx2::Reader::new(t.ktx2.as_slice()).expect("tile ktx2");
            let level = reader.levels().next().expect("tile mip 0").data;
            let rgba = if platform == "android" {
                let srgb = matches!(cooked.format, TileFormat::Bc7Srgb);
                regia_asset::astc_decode::decode_astc(cooked.tile_size, cooked.tile_size, (4, 4), srgb, level)
                    .expect("astc decode")
            } else {
                let kind = match cooked.format {
                    TileFormat::Bc7Srgb | TileFormat::Bc7Linear => regia_asset::bcn_decode::BcnKind::Bc7,
                    TileFormat::Bc5 => regia_asset::bcn_decode::BcnKind::Bc5,
                    TileFormat::Bc4 => regia_asset::bcn_decode::BcnKind::Bc4,
                };
                regia_asset::bcn_decode::decode_bcn(kind, cooked.tile_size, cooked.tile_size, level)
                    .expect("bcn decode")
            };
            for row in 0..tile {
                let dst = ((t.y as usize * tile + row) * size + t.x as usize * tile) * 4;
                out[dst..dst + tile * 4].copy_from_slice(&rgba[row * tile * 4..(row + 1) * tile * 4]);
            }
        }
        out
    }

    /// PSNR (dB) of `decoded` against `source` over the channels the slot stores.
    fn psnr(source: &[u8], decoded: &[u8], channels: &[usize]) -> f64 {
        let (mut sum, mut n) = (0.0f64, 0u64);
        for (s, d) in source.as_chunks::<4>().0.iter().zip(decoded.as_chunks::<4>().0) {
            for &c in channels {
                let e = f64::from(s[c]) - f64::from(d[c]);
                sum += e * e;
                n += 1;
            }
        }
        let mse = sum / n.max(1) as f64;
        if mse == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }

    /// Look parity (ADR-5294): on a natural-looking 512² source, every slot's Android ASTC 4×4
    /// mip 0 decodes no more than 1 dB PSNR below the desktop BCn tiles of the same source, both
    /// measured against the source over the channels the slot stores (BC5 RG, BC4 R). The
    /// criterion was fixed before the measurement. CPU only: the engine's reference decoders.
    #[test]
    fn android_astc_tiles_match_desktop_bcn_quality() {
        let size = 512;
        let cases = [
            (TileFormat::Bc7Srgb, &[0usize, 1, 2][..], "albedo"),
            (TileFormat::Bc7Linear, &[0, 1, 2, 3][..], "mask"),
            (TileFormat::Bc5, &[0, 1][..], "normal"),
            (TileFormat::Bc4, &[0][..], "height"),
        ];
        let mut report = Vec::new();
        for (format, channels, label) in cases {
            let source = terrain_like_rgba(size, format);
            let cook = |platform: &str| {
                cook_tiled_texture(
                    label,
                    size,
                    size,
                    &source,
                    256,
                    format,
                    platform,
                    TextureCookQuality::High,
                )
                .unwrap_or_else(|e| panic!("{label} {platform}: {e}"))
            };
            let bcn = psnr(&source, &decode_mip0(&cook("linux"), "linux"), channels);
            let astc = psnr(&source, &decode_mip0(&cook("android"), "android"), channels);
            report.push((label, bcn, astc));
        }
        let text: Vec<String> = report
            .iter()
            .map(|(label, bcn, astc)| format!("{label}: bcn {bcn:.2} dB, astc {astc:.2} dB"))
            .collect();
        eprintln!("terrain tile look parity (512², mip 0): {}", text.join("; "));
        for (label, bcn, astc) in &report {
            assert!(
                *astc >= bcn - 1.0,
                "{label}: ASTC 4x4 {astc:.2} dB is more than 1 dB below BCn {bcn:.2} dB"
            );
        }
    }

    /// The threaded tile encoder returns exactly what a serial loop over the same jobs returns:
    /// same order, same coordinates, same bytes (Android ASTC and desktop BCn).
    #[test]
    fn threaded_tile_encode_matches_a_serial_loop() {
        let (size, tile) = (256u32, 64u32);
        let source = terrain_like_rgba(size, TileFormat::Bc7Linear);
        let mut jobs = Vec::new();
        for (mip, (mw, _, data)) in generate_mips_rgba8(size, size, &source).iter().enumerate() {
            if *mw < tile {
                break;
            }
            for ty in 0..mw / tile {
                for tx in 0..mw / tile {
                    jobs.push((mip as u32, tx, ty, extract_tile(*mw, tile, data, tx, ty)));
                }
            }
        }
        assert_eq!(jobs.len(), 16 + 4 + 1);
        for platform in ["android", "linux"] {
            let threaded = encode_tiles(
                &jobs,
                tile,
                TileFormat::Bc7Linear,
                platform,
                TextureCookQuality::High,
            )
            .expect("threaded");
            let serial: Vec<TileCook> = jobs
                .iter()
                .map(|(mip, x, y, rgba)| TileCook {
                    mip: *mip,
                    x: *x,
                    y: *y,
                    ktx2: cook_chain(
                        tile,
                        tile,
                        &generate_mips_rgba8(tile, tile, rgba),
                        TileFormat::Bc7Linear,
                        platform,
                        TextureCookQuality::High,
                    )
                    .expect("serial"),
                })
                .collect();
            assert_eq!(threaded, serial, "{platform}");
        }
    }

    /// Finding 6 (fixrev-terrain-tiles-g): a threaded tile run caps each worker's inner
    /// encoders at one band (BCn `band_threads`, ASTC `strip_threads`), so a 512² or larger tile
    /// does not spawn up to 16 band threads per worker. A serial run keeps the full banding, and
    /// the caller's cap is untouched afterwards.
    #[test]
    fn threaded_tile_workers_encode_each_level_in_one_band() {
        use crate::astc_compress::strip_threads;
        use crate::texture_compress::{band_thread_cap, band_threads};
        // A 1024² level: banded by both encoders when uncapped (256 block rows).
        let bands = || (band_threads(1024, 1024), strip_threads(1024, 1024, 256));
        let uncapped = bands();
        let cores = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(16);
        assert_eq!(uncapped, (cores, cores), "uncapped banding on this host");
        let jobs: Vec<u32> = (0..8).collect();
        let threaded = run_jobs_in_order(&jobs, 4, |_| Ok::<_, ()>(bands())).expect("threaded run");
        assert_eq!(
            threaded,
            vec![(1, 1); 8],
            "one band per level inside a tile worker"
        );
        let serial = run_jobs_in_order(&jobs, 1, |_| Ok::<_, ()>(bands())).expect("serial run");
        assert_eq!(
            serial,
            vec![uncapped; 8],
            "a serial run keeps the encoders' banding"
        );
        assert_eq!(band_thread_cap(), usize::MAX, "the caller's cap is restored");
    }
}
