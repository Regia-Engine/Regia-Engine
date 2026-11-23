// SPDX-License-Identifier: MPL-2.0
//! Baked lightmaps (`Lightmap`, `.flmap`): one atlas of per-texel irradiance for the static meshes
//! of a map, with the second UV set (UV2) each placement was unwrapped with. Baked offline by
//! `regia bake-lightmaps` (`regia_pt::lightmap`, UV2 from `regia_asset_pipeline::lightmap_uv`),
//! sampled at runtime by `regia_render::lightmap` behind a flag (ADR-860,
//! `docs/formats/lightmap.md`).
//!
//! ## Layout
//!
//! - **Atlas**: `width × height` texels, row-major, top row first. A texel holds the irradiance
//!   (lux, linear RGB) arriving at the surface it covers, from the light components named by
//!   [`LightmapBakeInfo::contents`] ([`LightmapContents`]), encoded RGB9E5
//!   (`EXT_texture_shared_exponent`) after a multiply by [`Lightmap::exposure`], a power of two.
//!   The runtime shades `albedo / π · E` with it.
//! - **Coverage**: one byte per texel ([`Coverage`]): baked from a surface, filled by the gutter
//!   dilation, or empty.
//! - **Entries**: one [`LightmapEntry`] per placed mesh. Its rectangle in the atlas, the
//!   `scale_offset` that maps its local UV2 (0..1 over the rectangle) to atlas UV, and a
//!   [`MeshUnwrap`]: the unwrapped vertex list (`xref`: the source vertex each one copies, a
//!   chart seam splits a vertex), the local UV2 per unwrapped vertex, and the triangle list over
//!   the unwrapped vertices.
//!
//! - **Directions** (format version 2, optional): one RGBA8 texel per atlas texel
//!   ([`encode_direction`]): the dominant direction of the baked light and how much of it lines up
//!   with the surface normal, so a normal map can bend the baked light at runtime
//!   ([`shade_directional`], ADR-1556).
//!
//! The file is `FLMP`, the format version, the header length, a postcard [`LightmapHeader`],
//! padding to 16 bytes, then four raw little-endian sections (texels, coverage, geometry,
//! directions; the last one is empty for a non-directional lightmap) that a loader can upload
//! without decoding. A blake3 hash of the sections guards against truncation
//! and corruption, as in `.fgiv` ([`crate::baked_gi`]).
//!
//! ## Versions
//!
//! [`LIGHTMAP_MIN_VERSION`] ..= [`LIGHTMAP_FORMAT_VERSION`] read; a newer file is refused with
//! [`LightmapError::UnsupportedVersion`]. Version 1 (three sections, no directions) reads as a
//! lightmap without directions, version 2 (ADR-1556) as one whose entries have no key and no seam
//! figures, and [`migrate_lightmap_file`] rewrites either at the current version
//! (`regia upgrade-project`). Version 3 (ADR-2024) adds [`LightmapEntry::key`], the scene entity an
//! entry was baked for, and [`LightmapEntry::seams`], its chart-seam figures ([`seam_report`]).
//! Version 4 (ADR-2107) adds a section of opaque visibility-buffer clusters, one blob per entry
//! ([`Lightmap::meshlets`]; `regia bake-lightmaps --meshlets`); a file without them has no bytes
//! there, and an older file reads as one without them.
//! Version 5 (ADR-2314) adds block-compressed texels: [`LightmapHeader::texel_encoding`]
//! [`TEXEL_ENCODING_BC6H`] (desktop) or [`TEXEL_ENCODING_ASTC_HDR`] (phones) puts 16-byte 4×4 blocks
//! of `irradiance × exposure` in the texel section ([`crate::hdr_blocks`], [`Lightmap::blocks`];
//! `regia cook` writes them on request), and [`LightmapHeader::denoise`] records the denoiser's
//! settings when the bake ran it. A reader decodes the blocks into [`Lightmap::texels`] too, so every
//! CPU path reads one form. Files at encoding 1 and older versions load unchanged.
//! Version 6 (ADR-2656) adds [`LightmapHeader::source_hashes`]. Version 7 (ADR-3300) adds atlas
//! pages: [`LightmapHeader::pages`] pages of `width × height` texels each, and
//! [`LightmapEntry::page`], the page an entry's rectangle is in (`rect` stays in page texels). The
//! texel, coverage and direction sections hold the pages one after another (page-major: the rows
//! of page 0, then those of page 1, ...), and a block encoding encodes each page on its own, so a
//! one-page file's sections are the bytes an older version wrote. An older file reads as one page.
//! The plan for the next shape (UV2 in the cooked mesh) is in `docs/formats/lightmap.md`.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::baked_gi::{SectionRef, from_rgb9e5, rgb9e5};

thread_local! {
    static LIGHTMAP_ENTRY_CALLS: Cell<u64> = const { Cell::new(0) };
    static LIGHTMAP_ENTRY_SCANS: Cell<u64> = const { Cell::new(0) };
}

fn note_lightmap_entry_call() {
    LIGHTMAP_ENTRY_CALLS.with(|n| n.set(n.get().saturating_add(1)));
}

fn note_lightmap_entry_scan() {
    LIGHTMAP_ENTRY_SCANS.with(|n| n.set(n.get().saturating_add(1)));
}

/// Times [`Lightmap::entry`] ran on this thread since [`reset_lightmap_entry_counts`].
#[must_use]
pub fn lightmap_entry_calls() -> u64 {
    LIGHTMAP_ENTRY_CALLS.with(Cell::get)
}

/// Entry names or keys compared by [`Lightmap::entry`] and [`Lightmap::entry_for_key`] on this
/// thread since [`reset_lightmap_entry_counts`]. A hash hit is not a compare.
#[must_use]
pub fn lightmap_entry_scans() -> u64 {
    LIGHTMAP_ENTRY_SCANS.with(Cell::get)
}

/// Zeroes [`lightmap_entry_calls`] and [`lightmap_entry_scans`] on this thread.
pub fn reset_lightmap_entry_counts() {
    LIGHTMAP_ENTRY_CALLS.with(|n| n.set(0));
    LIGHTMAP_ENTRY_SCANS.with(|n| n.set(0));
}

mod receiver;
mod sampling;
mod seams;
mod v1;
mod v2;
mod v3;
mod v4;
mod v5;
mod v6;

pub use sampling::{decode_direction, encode_direction, shade_directional};
pub use seams::{
    ChartSeam, DEFAULT_HARD_EDGE_DEG, DEFAULT_SEAM_BUDGET, EntrySeamReport, SeamGeometry, SeamSegment,
    seam_report, seam_report_by_source_vertex,
};

/// Magic bytes of a lightmap file.
pub const LIGHTMAP_MAGIC: [u8; 4] = *b"FLMP";
/// Current format version.
pub const LIGHTMAP_FORMAT_VERSION: u32 = 7;
/// Oldest format version this build reads.
pub const LIGHTMAP_MIN_VERSION: u32 = 1;
/// [`LightmapHeader::texel_encoding`] of format version 1: RGB9E5, one `u32` per texel.
pub const TEXEL_ENCODING_RGB9E5: u32 = 1;
/// [`LightmapHeader::texel_encoding`] (version 5): BC6H unsigned-float 4×4 blocks
/// ([`crate::hdr_blocks`]), 16 bytes a block, row-major, partial blocks at the edges whole.
pub const TEXEL_ENCODING_BC6H: u32 = 2;
/// [`LightmapHeader::texel_encoding`] (version 5): ASTC 4×4 HDR blocks ([`crate::hdr_blocks`]),
/// laid out like [`TEXEL_ENCODING_BC6H`].
pub const TEXEL_ENCODING_ASTC_HDR: u32 = 3;
/// Bytes of one unwrapped vertex in the geometry section (`xref` u32, UV2 f32 × 2).
pub const UNWRAP_VERTEX_BYTES: usize = 12;
/// Largest atlas side the format accepts.
pub const MAX_LIGHTMAP_SIZE: u32 = 8192;
/// Most atlas pages a lightmap holds (format version 7, ADR-3300; a texture array of this many
/// layers fits every target's limit).
pub const MAX_LIGHTMAP_PAGES: u32 = 64;
/// How far apart pages sit in a packed `scale_offset`'s `z` ([`LightmapEntry::packed_scale_offset`]):
/// page `p` adds `p ×` this, so `floor(z / stride)` is the page and the rest the atlas U offset.
pub const LIGHTMAP_PAGE_U_STRIDE: f32 = 2.0;
/// Number of raw sections after the header (texels, coverage, geometry, directions).
pub const LIGHTMAP_SECTIONS: usize = 4;
/// [`LightmapHeader::direction_encoding`]: no direction section (version 1 files, or a bake
/// without `directional`).
pub const DIRECTION_NONE: u32 = 0;
/// [`LightmapHeader::direction_encoding`]: one RGBA8 `u32` per texel ([`encode_direction`]).
pub const DIRECTION_RGBA8: u32 = 1;
/// [`LightmapHeader::direction_encoding`]: the RGBA8 directions as BC7 (unorm) 4×4 blocks, 16 bytes
/// each, row-major over `⌈width / 4⌉ × ⌈height / 4⌉` (ADR-2635; desktop GPUs).
pub const DIRECTION_BC7: u32 = 2;
/// [`LightmapHeader::direction_encoding`]: the RGBA8 directions as ASTC 4×4 LDR (linear) blocks,
/// laid out as [`DIRECTION_BC7`]'s (ADR-2635; phones).
pub const DIRECTION_ASTC_LDR: u32 = 3;

/// How the direction section stores the directions ([`Lightmap::direction_blocks`], ADR-2635).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LightmapDirectionEncoding {
    /// One RGBA8 `u32` per texel ([`DIRECTION_RGBA8`]); what a bake writes.
    #[default]
    Rgba8,
    /// BC7 blocks ([`DIRECTION_BC7`]): 1 byte a texel.
    Bc7,
    /// ASTC 4×4 LDR blocks ([`DIRECTION_ASTC_LDR`]): 1 byte a texel.
    AstcLdr,
}

impl LightmapDirectionEncoding {
    /// Every encoding.
    pub const ALL: [Self; 3] = [Self::Rgba8, Self::Bc7, Self::AstcLdr];

    /// The header value.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::Rgba8 => DIRECTION_RGBA8,
            Self::Bc7 => DIRECTION_BC7,
            Self::AstcLdr => DIRECTION_ASTC_LDR,
        }
    }

    /// The encoding of a header value (`None` for [`DIRECTION_NONE`] and unknown values).
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.code() == code)
    }

    /// The lowercase name (`rgba8`, `bc7`, `astc_ldr`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Rgba8 => "rgba8",
            Self::Bc7 => "bc7",
            Self::AstcLdr => "astc_ldr",
        }
    }

    /// The encoding named `name` ([`Self::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.name() == name)
    }

    /// Bytes the direction section takes for a `width × height` atlas.
    ///
    /// A product that does not fit in `usize` saturates, so a caller can reject it instead of
    /// panicking on the multiply in a debug build.
    #[must_use]
    pub fn section_bytes(self, width: u32, height: u32) -> usize {
        match self {
            Self::Rgba8 => (width as usize).saturating_mul(height as usize).saturating_mul(4),
            Self::Bc7 | Self::AstcLdr => (width.div_ceil(4) as usize)
                .saturating_mul(height.div_ceil(4) as usize)
                .saturating_mul(16),
        }
    }

    /// Decodes `data` (this encoding's section for a `width × height` atlas) into one RGBA8 `u32`
    /// per texel ([`encode_direction`]'s packing).
    ///
    /// # Errors
    ///
    /// [`LightmapError::Payload`] when the blocks are short or do not decode.
    pub fn decode(self, width: u32, height: u32, data: &[u8]) -> Result<Vec<u32>, LightmapError> {
        let mut out = Vec::new();
        self.decode_into(width, height, data, &mut out)?;
        Ok(out)
    }

    /// Appends this page's packed RGBA8 texels to `out`. RGBA8 reads `data` in place.
    ///
    /// # Errors
    ///
    /// [`LightmapError::Payload`] when the blocks are short or do not decode.
    fn decode_into(
        self,
        width: u32,
        height: u32,
        data: &[u8],
        out: &mut Vec<u32>,
    ) -> Result<(), LightmapError> {
        match self {
            Self::Rgba8 => append_rgba8_texels(out, data, width, height),
            Self::Bc7 => {
                let rgba =
                    crate::bcn_decode::decode_bcn(crate::bcn_decode::BcnKind::Bc7, width, height, data)
                        .map_err(|e| LightmapError::Payload(format!("direction blocks: {e}")))?;
                append_rgba8_texels(out, &rgba, width, height)
            }
            Self::AstcLdr => {
                let rgba = crate::astc_decode::decode_astc(width, height, (4, 4), false, data)
                    .map_err(|e| LightmapError::Payload(format!("direction blocks: {e}")))?;
                append_rgba8_texels(out, &rgba, width, height)
            }
        }
    }
}

/// Packs RGBA8 bytes into `out` as little-endian `u32` texels.
fn append_rgba8_texels(
    out: &mut Vec<u32>,
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<(), LightmapError> {
    let n = width as usize * height as usize;
    if rgba.len() != n * 4 {
        return Err(LightmapError::Payload(format!(
            "direction section decodes to {} bytes for a {width}×{height} atlas",
            rgba.len()
        )));
    }
    out.reserve(n);
    for texel in rgba.as_chunks::<4>().0 {
        out.push(u32::from_le_bytes(*texel));
    }
    Ok(())
}

/// The block-compressed direction section ([`Lightmap::direction_blocks`], ADR-2635).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LightmapDirectionBlocks {
    /// [`LightmapDirectionEncoding::Bc7`] or [`LightmapDirectionEncoding::AstcLdr`].
    pub encoding: LightmapDirectionEncoding,
    /// The blocks, 16 bytes each, row-major over `⌈width / 4⌉ × ⌈height / 4⌉`.
    pub data: Vec<u8>,
}

/// Bits of [`LightmapBakeInfo::contents`]: which light the texels hold. Light that is not in the
/// lightmap stays live at runtime (the direct sun with its shadows, by default).
pub struct LightmapContents;

impl LightmapContents {
    /// Sky light, seen directly and bounced.
    pub const SKY: u32 = 1;
    /// Sun light after at least one bounce.
    pub const SUN_BOUNCE: u32 = 2;
    /// The sun seen directly (shadowed). Off by default: the runtime's sun stays live.
    pub const SUN_DIRECT: u32 = 4;
    /// Static emissive surfaces, seen directly and bounced.
    pub const EMISSIVE: u32 = 8;
    /// Analytic local lights (point, spot, sphere, quad), direct and bounced.
    pub const LOCAL_LIGHTS: u32 = 16;
    /// Every bit this build knows.
    pub const KNOWN: u32 =
        Self::SKY | Self::SUN_BOUNCE | Self::SUN_DIRECT | Self::EMISSIVE | Self::LOCAL_LIGHTS;

    /// Lower-case names of the bits set in `bits`, in bit order.
    #[must_use]
    pub fn names(bits: u32) -> Vec<&'static str> {
        [
            (Self::SKY, "sky"),
            (Self::SUN_BOUNCE, "sun_bounce"),
            (Self::SUN_DIRECT, "sun_direct"),
            (Self::EMISSIVE, "emissive"),
            (Self::LOCAL_LIGHTS, "local_lights"),
        ]
        .into_iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|(_, n)| n)
        .collect()
    }
}

/// How the texel section stores the atlas ([`LightmapHeader::texel_encoding`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LightmapTexelEncoding {
    /// One RGB9E5 `u32` per texel ([`TEXEL_ENCODING_RGB9E5`]); what a bake writes.
    #[default]
    Rgb9e5,
    /// BC6H unsigned-float blocks ([`TEXEL_ENCODING_BC6H`]): 1 byte a texel, for desktop GPUs.
    Bc6h,
    /// ASTC 4×4 HDR blocks ([`TEXEL_ENCODING_ASTC_HDR`]): 1 byte a texel, for phones whose GPU
    /// samples ASTC HDR (the renderer decodes on the CPU elsewhere).
    AstcHdr,
}

impl LightmapTexelEncoding {
    /// Every encoding.
    pub const ALL: [Self; 3] = [Self::Rgb9e5, Self::Bc6h, Self::AstcHdr];

    /// The header value.
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            Self::Rgb9e5 => TEXEL_ENCODING_RGB9E5,
            Self::Bc6h => TEXEL_ENCODING_BC6H,
            Self::AstcHdr => TEXEL_ENCODING_ASTC_HDR,
        }
    }

    /// The encoding of a header value.
    #[must_use]
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.code() == code)
    }

    /// The lowercase name (`rgb9e5`, `bc6h`, `astc_hdr`), as the agent protocol and the CLI
    /// spell it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Rgb9e5 => "rgb9e5",
            Self::Bc6h => "bc6h",
            Self::AstcHdr => "astc_hdr",
        }
    }

    /// The encoding named `name` ([`Self::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.name() == name)
    }

    /// The block format, `None` for RGB9E5.
    #[must_use]
    pub fn block_format(self) -> Option<crate::hdr_blocks::HdrBlockFormat> {
        match self {
            Self::Rgb9e5 => None,
            Self::Bc6h => Some(crate::hdr_blocks::HdrBlockFormat::Bc6h),
            Self::AstcHdr => Some(crate::hdr_blocks::HdrBlockFormat::AstcHdr4x4),
        }
    }

    /// Bytes the texel section takes for a `width × height` atlas.
    #[must_use]
    pub fn section_bytes(self, width: u32, height: u32) -> usize {
        match self {
            Self::Rgb9e5 => width as usize * height as usize * 4,
            Self::Bc6h | Self::AstcHdr => crate::hdr_blocks::hdr_block_bytes(width, height),
        }
    }
}

/// The block-compressed texel section of a version 5 lightmap ([`Lightmap::blocks`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LightmapBlocks {
    /// [`LightmapTexelEncoding::Bc6h`] or [`LightmapTexelEncoding::AstcHdr`].
    pub encoding: LightmapTexelEncoding,
    /// The blocks, 16 bytes each, row-major over `⌈width / 4⌉ × ⌈height / 4⌉`.
    pub data: Vec<u8>,
}

/// How far block-compressed texels are from the RGB9E5 texels they were encoded from
/// ([`Lightmap::encode_texels`]), over the texels that are not empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct LightmapEncodeReport {
    /// The encoding written.
    pub encoding: LightmapTexelEncoding,
    /// Bytes of the texel section now.
    pub bytes: usize,
    /// Bytes it took as RGB9E5.
    pub rgb9e5_bytes: usize,
    /// Texels compared (baked or dilated).
    pub texels: usize,
    /// Mean per-channel error relative to the texel's brightest channel.
    pub mean_rel: f32,
    /// 99th percentile of that error.
    pub p99_rel: f32,
    /// The largest.
    pub max_rel: f32,
}

/// The denoiser a bake ran (`regia_pt::lightmap`, ADR-2314), recorded in a version 5 header
/// ([`LightmapHeader::denoise`]); also the denoiser's settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LightmapDenoiseInfo {
    /// À-trous passes (tap spacing 1, 2, 4, ...), 1..=[`MAX_DENOISE_ITERATIONS`].
    pub iterations: u32,
    /// Luminance edge-stopping width in standard errors of the texels' measured noise (> 0).
    pub sigma_luminance: f32,
    /// Normal edge-stopping exponent: a tap weighs `max(0, n_p · n_q)^normal_power` (>= 0).
    pub normal_power: f32,
    /// Position edge-stopping width, texels of world distance beyond what the UV offset explains
    /// (> 0).
    pub sigma_position: f32,
}

/// Most denoiser passes ([`LightmapDenoiseInfo::iterations`]).
pub const MAX_DENOISE_ITERATIONS: u32 = 8;

impl Default for LightmapDenoiseInfo {
    fn default() -> Self {
        Self {
            iterations: 3,
            sigma_luminance: 4.0,
            normal_power: 32.0,
            sigma_position: 1.0,
        }
    }
}

impl LightmapDenoiseInfo {
    /// Checks the bounds; the message names the field first.
    ///
    /// # Errors
    ///
    /// A field outside its range.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_DENOISE_ITERATIONS).contains(&self.iterations) {
            return Err(format!(
                "iterations {} is outside 1..={MAX_DENOISE_ITERATIONS}",
                self.iterations
            ));
        }
        let positive = |v: f32| v.is_finite() && v > 0.0;
        if !positive(self.sigma_luminance) {
            return Err(format!(
                "sigma_luminance {} is not a positive number",
                self.sigma_luminance
            ));
        }
        if !(self.normal_power.is_finite() && self.normal_power >= 0.0) {
            return Err(format!("normal_power {} is not a number >= 0", self.normal_power));
        }
        if !positive(self.sigma_position) {
            return Err(format!(
                "sigma_position {} is not a positive number",
                self.sigma_position
            ));
        }
        Ok(())
    }
}

/// Sets every entry's counts and first vertex / index from its unwrap, in order.
fn assign_offsets(entries: &mut [LightmapEntry], unwraps: &[MeshUnwrap]) {
    let (mut v, mut i) = (0u32, 0u32);
    for (e, u) in entries.iter_mut().zip(unwraps) {
        e.first_vertex = v;
        e.first_index = i;
        e.vertex_count = u.xref.len() as u32;
        e.index_count = u.indices.len() as u32;
        v += e.vertex_count;
        i += e.index_count;
    }
}

/// Values of the coverage section.
pub struct Coverage;

impl Coverage {
    /// No surface: the texel is never sampled.
    pub const EMPTY: u8 = 0;
    /// The texel centre lies on a surface and was baked.
    pub const BAKED: u8 = 1;
    /// The texel is in a chart's gutter (or its centre was inside geometry) and holds the
    /// average of its baked neighbours, so bilinear filtering does not pull in black.
    pub const DILATED: u8 = 2;
}

/// One placed mesh in the atlas.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LightmapEntry {
    /// Placement name (the bake's instance name; unique within the file).
    pub name: String,
    /// Vertices of the source mesh (every `xref` is below this).
    pub source_vertices: u32,
    /// Rectangle in the atlas, texels: `[x, y, width, height]`, gutters included.
    pub rect: [u32; 4],
    /// Local UV2 → atlas UV: `atlas = local · [x, y] + [z, w]`.
    pub scale_offset: [f32; 4],
    /// Unwrapped vertices in the geometry section.
    pub vertex_count: u32,
    /// Unwrapped indices in the geometry section (a multiple of 3).
    pub index_count: u32,
    /// First unwrapped vertex of this entry in the geometry section.
    pub first_vertex: u32,
    /// First index of this entry in the geometry section.
    pub first_index: u32,
    /// Charts the unwrap made.
    pub charts: u32,
    /// World-space surface area, m².
    pub area_m2: f32,
    /// Texels per metre the entry was laid out at (after any atlas-fit scale).
    pub texels_per_metre: f32,
    /// The scene entity this entry was baked for (`regia bake-lightmaps --scene`, new in version
    /// 3); `None` for a built-in scene, a bake description or an older file.
    pub key: Option<LightmapEntryKey>,
    /// How well the entry's charts meet at their seams, measured when it was baked (new in
    /// version 3); `None` for an older file or an entry with no seam to measure.
    pub seams: Option<LightmapSeams>,
    /// The atlas page (texture array layer) the rectangle is in (new in version 7, ADR-3300); 0
    /// for a one-page lightmap and every older file. `rect` and `scale_offset` are in that page.
    pub page: u32,
}

impl LightmapEntry {
    /// [`Self::scale_offset`] with the page folded into `z` (`z + page × LIGHTMAP_PAGE_U_STRIDE`),
    /// the form a draw hands the GPU so the lookup picks the texture array layer from it
    /// (`shaders/lightmap.wgsl` `lightmap_page`). Page 0 returns `scale_offset` unchanged, bit for
    /// bit, so a one-page lightmap draws exactly as before pages existed.
    #[must_use]
    pub fn packed_scale_offset(&self) -> [f32; 4] {
        let s = self.scale_offset;
        if self.page == 0 {
            return s;
        }
        [s[0], s[1], s[2] + self.page as f32 * LIGHTMAP_PAGE_U_STRIDE, s[3]]
    }
}

/// The page and the in-page `scale_offset` of a packed one ([`LightmapEntry::packed_scale_offset`]).
#[must_use]
pub fn unpack_scale_offset(packed: [f32; 4]) -> (u32, [f32; 4]) {
    let page = (packed[2] / LIGHTMAP_PAGE_U_STRIDE).floor().max(0.0);
    let z = packed[2] - page * LIGHTMAP_PAGE_U_STRIDE;
    (page as u32, [packed[0], packed[1], z, packed[3]])
}

/// Pages a list of entries spans: one past the highest [`LightmapEntry::page`], at least 1.
#[must_use]
pub fn pages_of(entries: &[LightmapEntry]) -> u32 {
    entries
        .iter()
        .map(|e| e.page.saturating_add(1))
        .max()
        .unwrap_or(1)
        .max(1)
}

/// The stable identity of a placed mesh part: survives a re-bake, a re-order of the level and a
/// rename of the entity, unlike the entry's name for a bake description (`instance_<k>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LightmapEntryKey {
    /// The entity's guid (`regia_scene`'s `EntityGuid`; the scene file's `guid`).
    pub entity: regia_core::id::Guid,
    /// The part of the entity's mesh source, in the source's order (0 for a one-part mesh).
    pub part: u32,
}

/// One entry's chart-seam figures ([`seam_report`], stored by `regia bake-lightmaps`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LightmapSeams {
    /// Seam edges measured (edges where a smooth surface is cut in UV2).
    pub edges: u32,
    /// Length-weighted mean relative luminance step across a seam.
    pub mean: f32,
    /// Length-weighted 95th percentile of the step: the figure the seam budget limits.
    pub p95: f32,
    /// The largest step on any seam edge.
    pub max: f32,
    /// The chart holding the largest step, numbered as [`ChartSeam::chart`].
    pub worst_chart: u32,
}

/// The unwrap of one entry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MeshUnwrap {
    /// For each unwrapped vertex, the source vertex it copies.
    pub xref: Vec<u32>,
    /// Local UV2 per unwrapped vertex, 0..1 over the entry's rectangle.
    pub uv: Vec<[f32; 2]>,
    /// Triangle list over the unwrapped vertices, in the source's triangle order.
    pub indices: Vec<u32>,
}

impl MeshUnwrap {
    /// Number of triangles.
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
}

/// How the lightmap was baked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LightmapBakeInfo {
    /// [`LightmapContents`] bits.
    pub contents: u32,
    /// Hemisphere samples per texel.
    pub samples: u32,
    /// Path vertices after the texel (1 = direct light only).
    pub bounces: u32,
    /// Random seed.
    pub seed: u32,
    /// Unit vector toward the baked sun (zero without a sun).
    pub sun_toward: [f32; 3],
    /// Illuminance of the baked sun × its luminance-weighted colour, lux (0 without a sun).
    pub sun_illuminance: f32,
    /// The edge-aware filter ran.
    pub filtered: bool,
}

/// The postcard header of a `.flmap` file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LightmapHeader {
    /// Atlas width, texels.
    pub width: u32,
    /// Atlas height, texels.
    pub height: u32,
    /// Texel encoding: [`TEXEL_ENCODING_RGB9E5`], or (version 5) [`TEXEL_ENCODING_BC6H`] or
    /// [`TEXEL_ENCODING_ASTC_HDR`].
    pub texel_encoding: u32,
    /// Encode multiplier (a power of two): stored = irradiance × exposure.
    pub exposure: f32,
    /// Requested texel density, texels per metre (entries record what they got).
    pub density: f32,
    /// Gutter around every chart, texels.
    pub padding: u32,
    /// The placed meshes.
    pub entries: Vec<LightmapEntry>,
    /// Bake settings.
    pub bake: LightmapBakeInfo,
    /// Encoding of the direction section: [`DIRECTION_NONE`] or [`DIRECTION_RGBA8`] (new in
    /// version 2).
    pub direction_encoding: u32,
    /// Texels, coverage, geometry, directions (empty without directions).
    pub sections: [SectionRef; LIGHTMAP_SECTIONS],
    /// The cluster section (new in version 4; empty without clusters).
    pub meshlet_section: SectionRef,
    /// Bytes of each entry's cluster blob in the cluster section, in entry order (new in version
    /// 4; empty when no entry has clusters, else one per entry, 0 for an entry without).
    pub meshlet_bytes: Vec<u64>,
    /// blake3 of the sections, the cluster section last (an empty direction or cluster section
    /// adds nothing, so an older file keeps its hash).
    pub payload_hash: [u8; 32],
    /// The denoiser the bake ran (new in version 5; `None` when it did not run or for an older
    /// file).
    pub denoise: Option<LightmapDenoiseInfo>,
    /// The source mesh each entry was baked from ([`lightmap_source_hash`], new in version 6): empty
    /// when the bake recorded none, else one per entry in entry order, 0 for an entry without.
    pub source_hashes: Vec<u64>,
    /// Atlas pages, each `width × height` (new in version 7, ADR-3300; 1 for an older file): one
    /// past the highest entry page.
    pub pages: u32,
}

/// Lightmap read/write errors. [`Self::code`] is the agent error code.
#[derive(Debug, Error)]
pub enum LightmapError {
    /// Not a lightmap file, or the header runs past the end.
    #[error("not a lightmap file (bad magic or truncated header)")]
    BadMagic,
    /// A version this build cannot read.
    #[error("lightmap format version {found} is not supported (this build reads {min}..={supported})")]
    UnsupportedVersion {
        /// Version in the file.
        found: u32,
        /// Oldest version this build reads.
        min: u32,
        /// Newest version this build reads.
        supported: u32,
    },
    /// The header does not decode.
    #[error("lightmap header: {0}")]
    Header(#[from] postcard::Error),
    /// Sections that do not match the header, a hash mismatch or an unknown encoding.
    #[error("lightmap payload: {0}")]
    Payload(String),
    /// An inconsistent atlas or entry layout.
    #[error("lightmap layout: {0}")]
    Layout(String),
    /// File IO.
    #[error("lightmap io: {0}")]
    Io(#[from] std::io::Error),
}

impl LightmapError {
    /// The agent error code: `lightmap_bad_magic`, `lightmap_version`, `lightmap_header`,
    /// `lightmap_payload`, `lightmap_layout` or `lightmap_io` (`book/agents/lightmaps.md`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadMagic => "lightmap_bad_magic",
            Self::UnsupportedVersion { .. } => "lightmap_version",
            Self::Header(_) => "lightmap_header",
            Self::Payload(_) => "lightmap_payload",
            Self::Layout(_) => "lightmap_layout",
            Self::Io(_) => "lightmap_io",
        }
    }
}

/// What [`Lightmap::entry`] found in the name index.
enum EntryCache {
    /// The index is missing or does not match the entries.
    Stale,
    /// The name or key is in the index and the entry still says so.
    Found(usize),
    /// The index matches the entries and does not contain the name or key.
    Missing,
}

/// One built name and key index. `ptr` is the `entries` allocation it was built from.
struct EntryIndex {
    ptr: usize,
    len: usize,
    /// Pointers, lengths and keys of `entries` when the index was built.
    stamp: u64,
    by_name: HashMap<String, usize>,
    by_key: HashMap<LightmapEntryKey, usize>,
}

/// Name and key index for [`Lightmap::entry`]. It is not part of the `.flmap` file. A clone starts
/// empty. Any two indexes compare equal, so [`PartialEq`] of a lightmap ignores the cache.
#[derive(Default)]
struct EntryLookup {
    inner: RwLock<Option<EntryIndex>>,
}

impl Clone for EntryLookup {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for EntryLookup {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl std::fmt::Debug for EntryLookup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EntryLookup")
    }
}

/// The names (their bytes, not their allocations) and keys of `entries`. A name edited in place
/// keeps its pointer and length, and a name re-assigned twice can get its first allocation back,
/// so a pointer stamp would call a renamed entry missing.
fn entry_identity_stamp(entries: &[LightmapEntry]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for entry in entries {
        for &b in entry.name.as_bytes() {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= entry.name.len() as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        match entry.key {
            Some(key) => {
                let id = key.entity.as_u128();
                hash ^= id as u64;
                hash = hash.wrapping_mul(0x100000001b3);
                hash ^= (id >> 64) as u64;
                hash = hash.wrapping_mul(0x100000001b3);
                hash ^= u64::from(key.part);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            None => {
                hash ^= 0x9e3779b97f4a7c15;
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
    }
    hash
}

/// A baked lightmap atlas with its entries. A lightmap has [`Self::pages`] pages of `width ×
/// height` texels (format version 7, ADR-3300): the per-texel lists ([`Self::texels`],
/// [`Self::coverage`], [`Self::directions`]) hold them page after page, so a texel's index is
/// `(page × height + y) × width + x`, and the page count follows the entries
/// ([`LightmapEntry::page`], [`pages_of`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Lightmap {
    /// Page width, texels.
    pub width: u32,
    /// Page height, texels.
    pub height: u32,
    /// Encode multiplier (a power of two).
    pub exposure: f32,
    /// Requested texel density, texels per metre.
    pub density: f32,
    /// Gutter around every chart, texels.
    pub padding: u32,
    /// The placed meshes; `first_vertex` / `first_index` are rewritten on write.
    pub entries: Vec<LightmapEntry>,
    /// The unwrap of each entry (same order and length as [`Self::entries`]).
    pub unwraps: Vec<MeshUnwrap>,
    /// Bake settings.
    pub bake: LightmapBakeInfo,
    /// RGB9E5 texels of `irradiance × exposure`, row-major, top row first. For a block-compressed
    /// lightmap ([`Self::blocks`]) these are the decoded blocks, so every CPU reader sees what the
    /// GPU samples.
    pub texels: Vec<u32>,
    /// One [`Coverage`] value per texel.
    pub coverage: Vec<u8>,
    /// Dominant light direction per texel ([`encode_direction`]), row-major like
    /// [`Self::texels`]; empty for a non-directional lightmap.
    pub directions: Vec<u32>,
    /// Visibility-buffer clusters built for each entry's unwrap (ADR-2107): one opaque blob per
    /// entry (`regia_render::visbuffer::MeshletMesh::to_bytes`), empty for an entry without; the
    /// list is empty when no entry has any. This crate does not read the blobs. See
    /// [`Self::entry_meshlets`].
    pub meshlets: Vec<Vec<u8>>,
    /// The texel section as BC6H or ASTC HDR blocks (version 5, [`Self::encode_texels`]); `None`
    /// stores RGB9E5. [`Self::to_bytes`] writes these blocks, so [`Self::texels`] must be their
    /// decode (which [`Self::encode_texels`] and [`Self::from_bytes`] keep).
    pub blocks: Option<LightmapBlocks>,
    /// The denoiser the bake ran (version 5).
    pub denoise: Option<LightmapDenoiseInfo>,
    /// The direction section as BC7 or ASTC LDR blocks (ADR-2635, [`Self::set_direction_blocks`]);
    /// `None` stores RGBA8. [`Self::directions`] is their decode.
    pub direction_blocks: Option<LightmapDirectionBlocks>,
    /// The source mesh each entry was baked from ([`lightmap_source_hash`], version 6, ADR-2656):
    /// empty, or one per entry with 0 for an entry without. See [`Self::source_hash`].
    pub source_hashes: Vec<u64>,
    /// Name and key index. Not written to the file.
    lookup: EntryLookup,
}

/// The atlas layout a bake fills: size, density, entries and their unwraps
/// ([`Lightmap::from_irradiance`]). Its pages follow the entries ([`Self::pages`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LightmapLayout {
    /// Page width, texels.
    pub width: u32,
    /// Page height, texels.
    pub height: u32,
    /// Requested texel density, texels per metre.
    pub density: f32,
    /// Gutter around every chart, texels.
    pub padding: u32,
    /// The placed meshes.
    pub entries: Vec<LightmapEntry>,
    /// The unwrap of each entry.
    pub unwraps: Vec<MeshUnwrap>,
}

impl LightmapLayout {
    /// Pages the entries span ([`pages_of`]).
    #[must_use]
    pub fn pages(&self) -> u32 {
        pages_of(&self.entries)
    }

    /// The layout with its pages stacked into one tall atlas: page `p`'s rows follow page `p - 1`'s
    /// (height `pages × height`, every entry on page 0 with its rectangle moved down by `p ×
    /// height` and its `scale_offset` mapping into the tall atlas), the order [`Lightmap::texels`]
    /// keeps them in. A baker that works on one grid bakes this and hands the result to
    /// [`Lightmap::from_irradiance`] with the paged layout. A one-page layout comes back unchanged.
    /// The stacked height may pass [`MAX_LIGHTMAP_SIZE`]: it is a working grid, never a file.
    #[must_use]
    pub fn stacked(&self) -> Self {
        let mut tall = self.clone();
        let pages = self.pages();
        if pages <= 1 {
            return tall;
        }
        tall.height = self.height.saturating_mul(pages);
        let n = pages as f32;
        for e in &mut tall.entries {
            e.rect[1] = e.rect[1].saturating_add(e.page.saturating_mul(self.height));
            let s = e.scale_offset;
            e.scale_offset = [s[0], s[1] / n, s[2], (s[3] + e.page as f32) / n];
            e.page = 0;
        }
        tall
    }
}

impl Lightmap {
    /// Pages of the atlas ([`pages_of`] over the entries).
    #[must_use]
    pub fn pages(&self) -> u32 {
        pages_of(&self.entries)
    }

    /// Texels of one page (`width × height`). Saturates when the product does not fit in `usize`.
    #[must_use]
    pub fn page_texels(&self) -> usize {
        (self.width as usize).saturating_mul(self.height as usize)
    }

    /// `page` is inside the texel list: a constant-time check for the per-lookup readers, which
    /// [`Self::pages`] (a walk over every entry) would make linear in the entries.
    pub(crate) fn holds_page(&self, page: u32) -> bool {
        let one = self.page_texels();
        one > 0 && (page as usize + 1).saturating_mul(one) <= self.texels.len()
    }

    /// Texels over every page. Saturates when the product does not fit in `usize`.
    #[must_use]
    pub fn total_texels(&self) -> usize {
        self.page_texels().saturating_mul(self.pages() as usize)
    }

    /// Encodes linear irradiance (lux, one RGB per texel of `layout`'s atlas, page after page)
    /// with its coverage, picking the exposure from the brightest texel.
    ///
    /// # Errors
    ///
    /// [`LightmapError::Layout`] when the sizes disagree or an entry is inconsistent
    /// ([`Self::validate`]).
    pub fn from_irradiance(
        layout: LightmapLayout,
        irradiance: &[[f32; 3]],
        coverage: Vec<u8>,
        bake: LightmapBakeInfo,
    ) -> Result<Self, LightmapError> {
        let pages = layout.pages();
        let Some(n) = (layout.width as usize)
            .checked_mul(layout.height as usize)
            .and_then(|texels| texels.checked_mul(pages as usize))
        else {
            return Err(LightmapError::Layout(format!(
                "atlas {}×{} × {pages} pages does not fit",
                layout.width, layout.height
            )));
        };
        if irradiance.len() != n {
            return Err(LightmapError::Layout(format!(
                "{} irradiance texels for {pages} page(s) of {}×{}",
                irradiance.len(),
                layout.width,
                layout.height
            )));
        }
        let peak = irradiance
            .iter()
            .flat_map(|c| c.iter().copied())
            .filter(|v| v.is_finite())
            .fold(0.0f32, f32::max);
        let exposure = exposure_for(peak);
        let texels = irradiance
            .iter()
            .map(|c| rgb9e5([c[0] * exposure, c[1] * exposure, c[2] * exposure]))
            .collect();
        let mut map = Self {
            width: layout.width,
            height: layout.height,
            exposure,
            density: layout.density,
            padding: layout.padding,
            entries: layout.entries,
            unwraps: layout.unwraps,
            bake,
            texels,
            coverage,
            directions: Vec::new(),
            meshlets: Vec::new(),
            blocks: None,
            denoise: None,
            direction_blocks: None,
            source_hashes: Vec::new(),
            lookup: EntryLookup::default(),
        };
        map.assign_geometry_offsets();
        map.validate()?;
        Ok(map)
    }

    /// Rewrites every entry's counts and first vertex / index from [`Self::unwraps`].
    pub fn assign_geometry_offsets(&mut self) {
        assign_offsets(&mut self.entries, &self.unwraps);
    }

    /// Makes the lightmap directional: `directions` holds one `[x, y, z, w]` per texel, the
    /// luminance-weighted mean direction of the baked light (length 0..=1) and its agreement
    /// with the baked normal (`w`, 0..=1), encoded with [`encode_direction`].
    ///
    /// # Errors
    ///
    /// [`LightmapError::Layout`] when the count is not one per texel.
    pub fn set_directions(&mut self, directions: &[[f32; 4]]) -> Result<(), LightmapError> {
        let n = self.total_texels();
        if directions.len() != n {
            return Err(LightmapError::Layout(format!(
                "{} directions for {n} atlas texels",
                directions.len()
            )));
        }
        self.directions = directions.iter().map(|d| encode_direction(*d)).collect();
        self.direction_blocks = None;
        Ok(())
    }

    /// How the direction section is stored; `None` for a lightmap without directions.
    #[must_use]
    pub fn direction_encoding(&self) -> Option<LightmapDirectionEncoding> {
        if self.directions.is_empty() {
            return None;
        }
        Some(
            self.direction_blocks
                .as_ref()
                .map_or(LightmapDirectionEncoding::Rgba8, |b| b.encoding),
        )
    }

    /// Stores the direction section as `encoding`'s blocks `data` (encoded from [`Self::directions`]
    /// by the cook, ADR-2635): [`Self::directions`] becomes their decode, so every CPU reader sees
    /// what the GPU samples. [`LightmapDirectionEncoding::Rgba8`] drops the blocks and keeps the
    /// directions. Returns the largest per-channel change of a direction texel, in 8-bit levels.
    ///
    /// # Errors
    ///
    /// [`LightmapError::Layout`] for a lightmap without directions or blocks of the wrong size;
    /// [`LightmapError::Payload`] when they do not decode.
    pub fn set_direction_blocks(
        &mut self,
        encoding: LightmapDirectionEncoding,
        data: Vec<u8>,
    ) -> Result<u8, LightmapError> {
        if self.directions.is_empty() {
            return Err(LightmapError::Layout(
                "the lightmap has no directions to encode".into(),
            ));
        }
        if encoding == LightmapDirectionEncoding::Rgba8 {
            self.direction_blocks = None;
            return Ok(0);
        }
        let pages = self.pages() as usize;
        let per_page = encoding.section_bytes(self.width, self.height);
        let Some(need) = per_page.checked_mul(pages) else {
            return Err(LightmapError::Layout(format!(
                "direction blocks for {pages} page(s) of {}×{} do not fit",
                self.width, self.height
            )));
        };
        if data.len() != need {
            return Err(LightmapError::Layout(format!(
                "{} bytes of {} direction blocks for {pages} page(s) of {}×{}",
                data.len(),
                encoding.name(),
                self.width,
                self.height
            )));
        }
        let decoded = decode_direction_pages(encoding, self.width, self.height, pages, &data)?;
        let worst = self
            .directions
            .iter()
            .zip(&decoded)
            .map(|(a, b)| {
                (0..4)
                    .map(|k| ((a >> (8 * k)) & 0xff).abs_diff((b >> (8 * k)) & 0xff))
                    .max()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        self.directions = decoded;
        self.direction_blocks = Some(LightmapDirectionBlocks { encoding, data });
        Ok(u8::try_from(worst).unwrap_or(u8::MAX))
    }

    /// True when the lightmap carries a direction per texel.
    #[must_use]
    pub fn is_directional(&self) -> bool {
        !self.directions.is_empty()
    }

    /// Decoded direction of texel `(x, y)` ([`decode_direction`]); `None` without directions or
    /// outside the atlas.
    #[must_use]
    pub fn direction_at(&self, x: u32, y: u32) -> Option<[f32; 4]> {
        if x >= self.width {
            return None;
        }
        self.directions
            .get(y as usize * self.width as usize + x as usize)
            .map(|&d| decode_direction(d))
    }

    /// Checks the atlas size, the section lengths and every entry: its rectangle inside the
    /// atlas, rectangles that do not overlap, unique names, UV2 within 0..1, indices within the
    /// unwrap and `xref` within the source mesh.
    ///
    /// # Errors
    ///
    /// [`LightmapError::Layout`] naming the first problem.
    pub fn validate(&self) -> Result<(), LightmapError> {
        let bad = |m: String| Err(LightmapError::Layout(m));
        if self.width == 0
            || self.height == 0
            || self.width > MAX_LIGHTMAP_SIZE
            || self.height > MAX_LIGHTMAP_SIZE
        {
            return bad(format!(
                "atlas {}×{} is outside 1..={MAX_LIGHTMAP_SIZE}",
                self.width, self.height
            ));
        }
        let pages = self.pages();
        if pages > MAX_LIGHTMAP_PAGES {
            return bad(format!("{pages} pages, over {MAX_LIGHTMAP_PAGES}"));
        }
        let n = self.total_texels();
        if self.texels.len() != n || self.coverage.len() != n {
            return bad(format!(
                "{} texels and {} coverage bytes for {n} atlas texels",
                self.texels.len(),
                self.coverage.len()
            ));
        }
        if !self.directions.is_empty() && self.directions.len() != n {
            return bad(format!(
                "{} directions for {n} atlas texels",
                self.directions.len()
            ));
        }
        if let Some(c) = self.coverage.iter().find(|&&c| c > Coverage::DILATED) {
            return bad(format!("unknown coverage value {c}"));
        }
        if self.entries.len() != self.unwraps.len() {
            return bad(format!(
                "{} entries and {} unwraps",
                self.entries.len(),
                self.unwraps.len()
            ));
        }
        if !self.meshlets.is_empty() && self.meshlets.len() != self.entries.len() {
            return bad(format!(
                "{} cluster blobs for {} entries",
                self.meshlets.len(),
                self.entries.len()
            ));
        }
        // A subnormal exposure is positive but its reciprocal (the decode scale) is infinite.
        if !(self.exposure.is_finite() && self.exposure > 0.0 && (1.0 / self.exposure).is_finite()) {
            return bad(format!("exposure {} is not a positive number", self.exposure));
        }
        if self.bake.contents & !LightmapContents::KNOWN != 0 {
            return bad(format!("unknown contents bits {:#x}", self.bake.contents));
        }
        if let Some(b) = &self.blocks {
            let want = b.encoding.section_bytes(self.width, self.height) * pages as usize;
            if b.encoding == LightmapTexelEncoding::Rgb9e5 || b.data.len() != want {
                return bad(format!(
                    "{} bytes of {} blocks for a {}×{} atlas ({want} expected)",
                    b.data.len(),
                    b.encoding.name(),
                    self.width,
                    self.height
                ));
            }
        }
        if let Some(d) = &self.denoise {
            d.validate()
                .map_err(|m| LightmapError::Layout(format!("denoise: {m}")))?;
        }
        if !self.source_hashes.is_empty() && self.source_hashes.len() != self.entries.len() {
            return bad(format!(
                "{} source hashes for {} entries",
                self.source_hashes.len(),
                self.entries.len()
            ));
        }
        let mut names = std::collections::HashSet::new();
        for (k, (e, u)) in self.entries.iter().zip(&self.unwraps).enumerate() {
            if !names.insert(e.name.as_str()) {
                return bad(format!("entry {k}: duplicate name `{}`", e.name));
            }
            let [x, y, w, h] = e.rect;
            if w == 0
                || h == 0
                || x.checked_add(w).is_none_or(|r| r > self.width)
                || y.checked_add(h).is_none_or(|b| b > self.height)
            {
                return bad(format!(
                    "entry {k} `{}`: rect {:?} outside the atlas",
                    e.name, e.rect
                ));
            }
            for (j, o) in self.entries[..k].iter().enumerate() {
                if o.page == e.page && rects_overlap(e.rect, o.rect) {
                    return bad(format!("entry {k} `{}` overlaps entry {j} `{}`", e.name, o.name));
                }
            }
            if u.xref.len() != u.uv.len() {
                return bad(format!(
                    "entry {k} `{}`: {} xrefs and {} UVs",
                    e.name,
                    u.xref.len(),
                    u.uv.len()
                ));
            }
            if !u.indices.len().is_multiple_of(3) {
                return bad(format!(
                    "entry {k} `{}`: index count is not a multiple of 3",
                    e.name
                ));
            }
            if u.indices.iter().any(|&i| i as usize >= u.xref.len()) {
                return bad(format!(
                    "entry {k} `{}`: an index is past the unwrapped vertices",
                    e.name
                ));
            }
            if u.xref.iter().any(|&s| s >= e.source_vertices) {
                return bad(format!(
                    "entry {k} `{}`: an xref is past the source vertices",
                    e.name
                ));
            }
            if u.uv
                .iter()
                .flatten()
                .any(|v| !v.is_finite() || *v < 0.0 || *v > 1.0)
            {
                return bad(format!("entry {k} `{}`: a UV2 is outside 0..1", e.name));
            }
            if e.scale_offset.iter().any(|v| !v.is_finite()) {
                return bad(format!("entry {k} `{}`: scale_offset is not finite", e.name));
            }
        }
        Ok(())
    }

    /// The entry named `name`.
    #[must_use]
    pub fn entry(&self, name: &str) -> Option<usize> {
        note_lightmap_entry_call();
        self.lookup_name(name)
    }

    /// The entry baked for `key` (format version 3).
    #[must_use]
    pub fn entry_for_key(&self, key: LightmapEntryKey) -> Option<usize> {
        self.lookup_key(key)
    }

    fn lookup_name(&self, name: &str) -> Option<usize> {
        for _ in 0..2 {
            match self.cached_name(name) {
                EntryCache::Found(index) => return Some(index),
                EntryCache::Missing => return None,
                EntryCache::Stale => self.rebuild_entry_index(),
            }
        }
        self.entries.iter().position(|entry| {
            note_lightmap_entry_scan();
            entry.name == name
        })
    }

    fn lookup_key(&self, key: LightmapEntryKey) -> Option<usize> {
        for _ in 0..2 {
            match self.cached_key(key) {
                EntryCache::Found(index) => return Some(index),
                EntryCache::Missing => return None,
                EntryCache::Stale => self.rebuild_entry_index(),
            }
        }
        self.entries.iter().position(|entry| {
            note_lightmap_entry_scan();
            entry.key == Some(key)
        })
    }

    fn cached_name(&self, name: &str) -> EntryCache {
        let ptr = self.entries.as_ptr() as usize;
        let guard = self
            .lookup
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(index) = guard.as_ref() else {
            return EntryCache::Stale;
        };
        if index.ptr != ptr || index.len != self.entries.len() {
            return EntryCache::Stale;
        }
        if let Some(&slot) = index.by_name.get(name) {
            if self.entries.get(slot).is_some_and(|entry| entry.name == name) {
                return EntryCache::Found(slot);
            }
            return EntryCache::Stale;
        }
        if index.stamp == entry_identity_stamp(&self.entries) {
            EntryCache::Missing
        } else {
            EntryCache::Stale
        }
    }

    fn cached_key(&self, key: LightmapEntryKey) -> EntryCache {
        let ptr = self.entries.as_ptr() as usize;
        let guard = self
            .lookup
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(index) = guard.as_ref() else {
            return EntryCache::Stale;
        };
        if index.ptr != ptr || index.len != self.entries.len() {
            return EntryCache::Stale;
        }
        if let Some(&slot) = index.by_key.get(&key) {
            if self.entries.get(slot).is_some_and(|entry| entry.key == Some(key)) {
                return EntryCache::Found(slot);
            }
            return EntryCache::Stale;
        }
        if index.stamp == entry_identity_stamp(&self.entries) {
            EntryCache::Missing
        } else {
            EntryCache::Stale
        }
    }

    fn rebuild_entry_index(&self) {
        let mut by_name = HashMap::with_capacity(self.entries.len());
        let mut by_key = HashMap::new();
        for (slot, entry) in self.entries.iter().enumerate() {
            note_lightmap_entry_scan();
            by_name.entry(entry.name.clone()).or_insert(slot);
            if let Some(key) = entry.key {
                by_key.entry(key).or_insert(slot);
            }
        }
        let index = EntryIndex {
            ptr: self.entries.as_ptr() as usize,
            len: self.entries.len(),
            stamp: entry_identity_stamp(&self.entries),
            by_name,
            by_key,
        };
        *self
            .lookup
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(index);
    }

    /// The index of texel `(x, y)` of `page` in the per-texel lists; `None` outside the atlas.
    #[must_use]
    pub fn texel_index(&self, page: u32, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height || !self.holds_page(page) {
            return None;
        }
        Some((page as usize * self.height as usize + y as usize) * self.width as usize + x as usize)
    }

    /// Decoded irradiance (lux) of texel `(x, y)`, where `y` counts the stacked rows of every page
    /// (`page × height + row`; page 0's rows are `0..height`); black outside the atlas.
    #[must_use]
    pub fn irradiance_at(&self, x: u32, y: u32) -> [f32; 3] {
        let Some(&t) = (x < self.width)
            .then(|| self.texels.get(y as usize * self.width as usize + x as usize))
            .flatten()
        else {
            return [0.0; 3];
        };
        let c = from_rgb9e5(t);
        let inv = 1.0 / self.exposure;
        [c[0] * inv, c[1] * inv, c[2] * inv]
    }

    /// Coverage of texel `(x, y)`, `y` a stacked row as in [`Self::irradiance_at`]
    /// ([`Coverage::EMPTY`] outside the atlas).
    #[must_use]
    pub fn coverage_at(&self, x: u32, y: u32) -> u8 {
        if x >= self.width {
            return Coverage::EMPTY;
        }
        self.coverage
            .get(y as usize * self.width as usize + x as usize)
            .copied()
            .unwrap_or(Coverage::EMPTY)
    }

    /// Bilinear irradiance at atlas UV (`0..1`, texel centres at `(i + 0.5) / width`) on page 0,
    /// the lookup the runtime's sampler does (`shaders/lightmap.wgsl`). Empty texels get weight 0
    /// and the rest renormalise, so a lookup next to a chart edge does not darken; all four empty
    /// gives black. [`Self::sample_page`] reads another page.
    #[must_use]
    pub fn sample(&self, uv: [f32; 2]) -> [f32; 3] {
        self.sample_page(0, uv)
    }

    /// [`Self::sample`] on page `page`: the filter never reads another page (a texture array's
    /// layers do not filter into each other). Black past the last page.
    #[must_use]
    pub fn sample_page(&self, page: u32, uv: [f32; 2]) -> [f32; 3] {
        if !self.holds_page(page) {
            return [0.0; 3];
        }
        let row0 = page * self.height;
        let fx = uv[0] * self.width as f32 - 0.5;
        let fy = uv[1] * self.height as f32 - 0.5;
        let (x0, y0) = (fx.floor(), fy.floor());
        let (tx, ty) = (fx - x0, fy - y0);
        let clamp = |v: f32, n: u32| v.clamp(0.0, (n - 1) as f32) as u32;
        let mut sum = [0.0f32; 3];
        let mut wsum = 0.0f32;
        for (dx, dy, w) in [
            (0.0, 0.0, (1.0 - tx) * (1.0 - ty)),
            (1.0, 0.0, tx * (1.0 - ty)),
            (0.0, 1.0, (1.0 - tx) * ty),
            (1.0, 1.0, tx * ty),
        ] {
            let x = clamp(x0 + dx, self.width);
            let y = row0 + clamp(y0 + dy, self.height);
            if self.coverage_at(x, y) == Coverage::EMPTY || w <= 0.0 {
                continue;
            }
            let c = self.irradiance_at(x, y);
            for (s, c) in sum.iter_mut().zip(c) {
                *s += w * c;
            }
            wsum += w;
        }
        if wsum <= 0.0 {
            return [0.0; 3];
        }
        [sum[0] / wsum, sum[1] / wsum, sum[2] / wsum]
    }

    /// [`Self::sample`] at an entry's local UV2.
    #[must_use]
    pub fn sample_entry(&self, entry: usize, local: [f32; 2]) -> [f32; 3] {
        let Some(e) = self.entries.get(entry) else {
            return [0.0; 3];
        };
        let s = e.scale_offset;
        self.sample_page(e.page, [local[0] * s[0] + s[2], local[1] * s[1] + s[3]])
    }

    /// Texels per coverage value: `[empty, baked, dilated]`.
    #[must_use]
    pub fn coverage_counts(&self) -> [usize; 3] {
        let mut c = [0usize; 3];
        for &v in &self.coverage {
            if let Some(slot) = c.get_mut(v as usize) {
                *slot += 1;
            }
        }
        c
    }

    /// Share of the atlas holding baked or dilated texels, 0..1.
    #[must_use]
    pub fn utilization(&self) -> f32 {
        let [empty, ..] = self.coverage_counts();
        let n = self.coverage.len().max(1);
        (n - empty) as f32 / n as f32
    }

    fn geometry_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for u in &self.unwraps {
            for (x, uv) in u.xref.iter().zip(&u.uv) {
                out.extend_from_slice(&x.to_le_bytes());
                out.extend_from_slice(&uv[0].to_le_bytes());
                out.extend_from_slice(&uv[1].to_le_bytes());
            }
        }
        for u in &self.unwraps {
            for i in &u.indices {
                out.extend_from_slice(&i.to_le_bytes());
            }
        }
        out
    }

    /// Serializes the lightmap in the current format ([`LIGHTMAP_FORMAT_VERSION`]).
    ///
    /// # Errors
    ///
    /// An inconsistent layout ([`Self::validate`]) or a header that fails to encode.
    pub fn to_bytes(&self) -> Result<Vec<u8>, LightmapError> {
        // The offsets are settled on a copy of the entries only: the atlas is not cloned.
        self.validate()?;
        let mut entries = self.entries.clone();
        assign_offsets(&mut entries, &self.unwraps);
        let texels: Vec<u8> = match &self.blocks {
            Some(b) => b.data.clone(),
            None => self.texels.iter().flat_map(|t| t.to_le_bytes()).collect(),
        };
        let geometry = self.geometry_bytes();
        let directions: Vec<u8> = match &self.direction_blocks {
            Some(b) if !self.directions.is_empty() => b.data.clone(),
            _ => self.directions.iter().flat_map(|t| t.to_le_bytes()).collect(),
        };
        let bodies: [&[u8]; LIGHTMAP_SECTIONS] = [&texels, &self.coverage, &geometry, &directions];
        let clusters = self.meshlets.concat();
        let mut hasher = blake3::Hasher::new();
        for b in bodies {
            hasher.update(b);
        }
        hasher.update(&clusters);
        let mut header = LightmapHeader {
            width: self.width,
            height: self.height,
            texel_encoding: self.texel_encoding().code(),
            exposure: self.exposure,
            density: self.density,
            padding: self.padding,
            entries,
            bake: self.bake,
            direction_encoding: self
                .direction_encoding()
                .map_or(DIRECTION_NONE, LightmapDirectionEncoding::code),
            sections: [SectionRef::default(); LIGHTMAP_SECTIONS],
            meshlet_section: SectionRef::default(),
            meshlet_bytes: self.meshlets.iter().map(|b| b.len() as u64).collect(),
            payload_hash: *hasher.finalize().as_bytes(),
            denoise: self.denoise,
            source_hashes: self.source_hashes.clone(),
            pages: self.pages(),
        };
        // Section offsets depend on the header length, which depends on the offsets' varint
        // sizes: settle them (at most a few rounds), as `.fgiv` does.
        let mut header_bytes = postcard::to_allocvec(&header)?;
        for _ in 0..4 {
            let mut at = align16(12 + header_bytes.len()) as u64;
            for (s, b) in header.sections.iter_mut().zip(bodies) {
                *s = SectionRef {
                    offset: at,
                    len: b.len() as u64,
                };
                at = align16((at + b.len() as u64) as usize) as u64;
            }
            header.meshlet_section = SectionRef {
                offset: at,
                len: clusters.len() as u64,
            };
            let next = postcard::to_allocvec(&header)?;
            let settled = next.len() == header_bytes.len();
            header_bytes = next;
            if settled {
                break;
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(&LIGHTMAP_MAGIC);
        out.extend_from_slice(&LIGHTMAP_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&header_bytes);
        for (s, b) in header.sections.iter().zip(bodies) {
            if out.len() as u64 > s.offset {
                return Err(LightmapError::Payload("section offsets did not settle".into()));
            }
            out.resize(s.offset as usize, 0);
            out.extend_from_slice(b);
        }
        out.resize(header.meshlet_section.offset as usize, 0);
        out.extend_from_slice(&clusters);
        out.resize(align16(out.len()), 0);
        Ok(out)
    }

    /// Reads the header of a lightmap file without its sections (for listings, the agent
    /// inspector and the seam budget). An older header comes back in the current shape: version 1
    /// with [`DIRECTION_NONE`] and an empty direction section, versions 1 and 2 with no entry key
    /// and no seam figures, versions 1 to 3 with an empty cluster section, versions 1 to 4 with no
    /// denoise record, versions 1 to 5 with no source hashes, versions 1 to 6 as one page.
    ///
    /// # Errors
    ///
    /// Bad magic, an unsupported version, or a header that does not decode.
    pub fn read_header(bytes: &[u8]) -> Result<LightmapHeader, LightmapError> {
        let version = lightmap_version(bytes)?;
        if !(LIGHTMAP_MIN_VERSION..=LIGHTMAP_FORMAT_VERSION).contains(&version) {
            return Err(LightmapError::UnsupportedVersion {
                found: version,
                min: LIGHTMAP_MIN_VERSION,
                supported: LIGHTMAP_FORMAT_VERSION,
            });
        }
        let header_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        let end = 12usize.checked_add(header_len).ok_or(LightmapError::BadMagic)?;
        let header_bytes = bytes.get(12..end).ok_or(LightmapError::BadMagic)?;
        match version {
            1 => Ok(postcard::from_bytes::<v1::LightmapHeaderV1>(header_bytes)?.into()),
            2 => Ok(postcard::from_bytes::<v2::LightmapHeaderV2>(header_bytes)?.into()),
            3 => Ok(postcard::from_bytes::<v3::LightmapHeaderV3>(header_bytes)?.into()),
            4 => Ok(postcard::from_bytes::<v4::LightmapHeaderV4>(header_bytes)?.into()),
            5 => Ok(postcard::from_bytes::<v5::LightmapHeaderV5>(header_bytes)?.into()),
            6 => Ok(postcard::from_bytes::<v6::LightmapHeaderV6>(header_bytes)?.into()),
            _ => Ok(postcard::from_bytes(header_bytes)?),
        }
    }

    /// Reads a lightmap written by [`Self::to_bytes`].
    ///
    /// # Errors
    ///
    /// Bad magic, an unsupported version, a header that does not decode, sections that do not
    /// match the header, a hash mismatch, or an inconsistent layout.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, LightmapError> {
        let version = lightmap_version(bytes)?;
        let header = Self::read_header(bytes)?;
        let encoding = LightmapTexelEncoding::from_code(header.texel_encoding)
            .filter(|e| *e == LightmapTexelEncoding::Rgb9e5 || version >= 5)
            .ok_or_else(|| {
                LightmapError::Payload(format!("unknown texel encoding {}", header.texel_encoding))
            })?;
        let section = |k: usize| -> Result<&[u8], LightmapError> {
            let s = header.sections[k];
            let end = s.offset.checked_add(s.len).ok_or(LightmapError::BadMagic)?;
            // `usize::try_from`, not `as usize`: on a 32-bit target a large offset would
            // truncate and alias the header (the `.fgiv` reader already converts this way).
            let start = usize::try_from(s.offset).map_err(|_| {
                LightmapError::Payload(format!("section {k} offset does not fit this platform"))
            })?;
            let end = usize::try_from(end)
                .map_err(|_| LightmapError::Payload(format!("section {k} runs past the end of the file")))?;
            bytes
                .get(start..end)
                .ok_or_else(|| LightmapError::Payload(format!("section {k} runs past the end of the file")))
        };
        let (texels, coverage, geometry) = (section(0)?, section(1)?, section(2)?);
        let directions = section(3)?;
        let clusters = {
            let s = header.meshlet_section;
            let end = s.offset.checked_add(s.len).ok_or(LightmapError::BadMagic)?;
            let start = usize::try_from(s.offset).map_err(|_| {
                LightmapError::Payload("the cluster offset does not fit this platform".into())
            })?;
            let end = usize::try_from(end).map_err(|_| {
                LightmapError::Payload("the cluster section runs past the end of the file".into())
            })?;
            bytes.get(start..end).ok_or_else(|| {
                LightmapError::Payload("the cluster section runs past the end of the file".into())
            })?
        };
        let mut hasher = blake3::Hasher::new();
        for b in [texels, coverage, geometry, directions, clusters] {
            hasher.update(b);
        }
        if *hasher.finalize().as_bytes() != header.payload_hash {
            return Err(LightmapError::Payload(
                "payload hash mismatch (corrupt file)".into(),
            ));
        }
        // The size cap comes before any product: a `u32::MAX` square overflows `n * 4`.
        if header.width == 0
            || header.height == 0
            || header.width > MAX_LIGHTMAP_SIZE
            || header.height > MAX_LIGHTMAP_SIZE
        {
            return Err(LightmapError::Layout(format!(
                "atlas {}×{} is outside 1..={MAX_LIGHTMAP_SIZE}",
                header.width, header.height
            )));
        }
        let pages = header.pages;
        if pages == 0 || pages > MAX_LIGHTMAP_PAGES || pages != pages_of(&header.entries) {
            return Err(LightmapError::Layout(format!(
                "{pages} pages for entries on {} (1..={MAX_LIGHTMAP_PAGES})",
                pages_of(&header.entries)
            )));
        }
        let page_count = pages as usize;
        let n = header.width as usize * header.height as usize * page_count;
        if texels.len() != encoding.section_bytes(header.width, header.height) * page_count
            || coverage.len() != n
        {
            return Err(LightmapError::Payload(format!(
                "section sizes do not match {pages} page(s) of {}×{}",
                header.width, header.height
            )));
        }
        let direction_encoding = match header.direction_encoding {
            DIRECTION_NONE => None,
            code => match LightmapDirectionEncoding::from_code(code) {
                Some(e) => Some(e),
                None => {
                    return Err(LightmapError::Payload(format!(
                        "unknown direction encoding {code}"
                    )));
                }
            },
        };
        let want_directions =
            direction_encoding.map_or(0, |e| e.section_bytes(header.width, header.height) * page_count);
        if directions.len() != want_directions {
            return Err(LightmapError::Payload(format!(
                "direction section holds {} bytes, {want_directions} expected",
                directions.len()
            )));
        }
        // The sums run in `u64`: on a 32-bit `usize` the entry counts and the byte products
        // wrap, a small geometry section compares equal to a huge forged count, and the
        // record slice below panics.
        let mut vertices = 0u64;
        let mut indices = 0u64;
        for e in &header.entries {
            vertices = vertices
                .checked_add(u64::from(e.vertex_count))
                .ok_or_else(|| LightmapError::Payload("geometry vertex count overflows".into()))?;
            indices = indices
                .checked_add(u64::from(e.index_count))
                .ok_or_else(|| LightmapError::Payload("geometry index count overflows".into()))?;
        }
        let vertex_bytes = vertices
            .checked_mul(UNWRAP_VERTEX_BYTES as u64)
            .ok_or_else(|| LightmapError::Payload("geometry byte size overflows".into()))?;
        let geometry_bytes = indices
            .checked_mul(4)
            .and_then(|index_bytes| vertex_bytes.checked_add(index_bytes))
            .ok_or_else(|| LightmapError::Payload("geometry byte size overflows".into()))?;
        if geometry_bytes != geometry.len() as u64 {
            return Err(LightmapError::Payload(format!(
                "geometry section holds {} bytes for {vertices} vertices and {indices} indices",
                geometry.len()
            )));
        }
        let vertex_bytes = usize::try_from(vertex_bytes)
            .map_err(|_| LightmapError::Payload("geometry does not fit this platform".into()))?;
        let (vertex_bytes, index_bytes) = geometry.split_at(vertex_bytes);
        let records = vertex_bytes.as_chunks::<UNWRAP_VERTEX_BYTES>().0;
        let index_words = index_bytes.as_chunks::<4>().0;
        let mut unwraps = Vec::with_capacity(header.entries.len());
        let (mut v_at, mut i_at) = (0usize, 0usize);
        for e in &header.entries {
            if e.first_vertex as usize != v_at || e.first_index as usize != i_at {
                return Err(LightmapError::Payload(format!(
                    "entry `{}` geometry offsets are not contiguous",
                    e.name
                )));
            }
            let vs = records.get(v_at..v_at + e.vertex_count as usize).ok_or_else(|| {
                LightmapError::Payload(format!("entry `{}` vertices run past the section", e.name))
            })?;
            let is = index_words
                .get(i_at..i_at + e.index_count as usize)
                .ok_or_else(|| {
                    LightmapError::Payload(format!("entry `{}` indices run past the section", e.name))
                })?;
            let word = |b: &[u8]| [b[0], b[1], b[2], b[3]];
            unwraps.push(MeshUnwrap {
                xref: vs.iter().map(|r| u32::from_le_bytes(word(&r[0..4]))).collect(),
                uv: vs
                    .iter()
                    .map(|r| {
                        [
                            f32::from_le_bytes(word(&r[4..8])),
                            f32::from_le_bytes(word(&r[8..12])),
                        ]
                    })
                    .collect(),
                indices: is.iter().map(|b| u32::from_le_bytes(*b)).collect(),
            });
            v_at += e.vertex_count as usize;
            i_at += e.index_count as usize;
        }
        let meshlets = split_meshlets(clusters, &header.meshlet_bytes)?;
        let (texel_words, blocks) = match encoding.block_format() {
            None => (
                texels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|b| u32::from_le_bytes(*b))
                    .collect(),
                None,
            ),
            Some(format) => {
                // Quantize each decoded texel straight into the RGB9E5 atlas. The float image
                // is the encoder's working copy, not something the loader keeps.
                let mut words = vec![0u32; n];
                let per_page = texels.len() / page_count;
                let page_texels = header.width as usize * header.height as usize;
                for (page_i, page) in texels.chunks(per_page.max(1)).enumerate() {
                    let base = page_i * page_texels;
                    crate::hdr_blocks::visit_hdr_blocks(
                        format,
                        header.width,
                        header.height,
                        page,
                        |i, px| {
                            words[base + i] = rgb9e5(px);
                        },
                    )
                    .map_err(|e| LightmapError::Payload(e.to_string()))?;
                }
                (
                    words,
                    Some(LightmapBlocks {
                        encoding,
                        data: texels.to_vec(),
                    }),
                )
            }
        };
        let map = Self {
            width: header.width,
            height: header.height,
            exposure: header.exposure,
            density: header.density,
            padding: header.padding,
            entries: header.entries,
            unwraps,
            bake: header.bake,
            texels: texel_words,
            coverage: coverage.to_vec(),
            directions: match direction_encoding {
                None => Vec::new(),
                Some(e) => decode_direction_pages(e, header.width, header.height, page_count, directions)?,
            },
            meshlets,
            blocks,
            denoise: header.denoise,
            direction_blocks: direction_encoding
                .filter(|e| *e != LightmapDirectionEncoding::Rgba8)
                .map(|encoding| LightmapDirectionBlocks {
                    encoding,
                    data: directions.to_vec(),
                }),
            source_hashes: header.source_hashes,
            lookup: EntryLookup::default(),
        };
        map.validate()?;
        Ok(map)
    }

    /// How the texel section is stored ([`Self::blocks`]).
    #[must_use]
    pub fn texel_encoding(&self) -> LightmapTexelEncoding {
        self.blocks
            .as_ref()
            .map_or(LightmapTexelEncoding::Rgb9e5, |b| b.encoding)
    }

    /// Re-encodes the texel section as `encoding` ([`crate::hdr_blocks`]): the blocks are encoded
    /// from the current texels (empty texels do not pull the fit), and [`Self::texels`] becomes
    /// their decode, so the CPU readers see what the GPU samples. [`LightmapTexelEncoding::Rgb9e5`]
    /// drops the blocks and keeps the texels as they are. Encoding twice re-encodes from the
    /// decoded texels (one more generation of loss): encode from the baked file.
    ///
    /// # Errors
    ///
    /// [`LightmapError::Payload`] when the encoder's output does not decode (a bug, never input).
    pub fn encode_texels(
        &mut self,
        encoding: LightmapTexelEncoding,
    ) -> Result<LightmapEncodeReport, LightmapError> {
        let rgb9e5_bytes = self.texels.len() * 4;
        let Some(format) = encoding.block_format() else {
            self.blocks = None;
            return Ok(LightmapEncodeReport {
                encoding,
                bytes: rgb9e5_bytes,
                rgb9e5_bytes,
                texels: self.coverage.iter().filter(|c| **c != Coverage::EMPTY).count(),
                ..LightmapEncodeReport::default()
            });
        };
        let source: Vec<[f32; 3]> = self.texels.iter().map(|&t| from_rgb9e5(t)).collect();
        let mask: Vec<bool> = self.coverage.iter().map(|c| *c != Coverage::EMPTY).collect();
        // Each page is its own image: blocks never span two pages. The decode is visited
        // texel by texel, so the error samples and the RGB9E5 words are the only copies.
        let page = self.page_texels().max(1);
        let mut data = Vec::new();
        let mut words = vec![0u32; source.len()];
        let mut rel = Vec::with_capacity(mask.iter().filter(|c| **c).count() * 3);
        for (page_i, (page_texels, page_mask)) in source.chunks(page).zip(mask.chunks(page)).enumerate() {
            let blocks = crate::hdr_blocks::encode_hdr_blocks(
                format,
                self.width,
                self.height,
                page_texels,
                Some(page_mask),
            );
            let base = page_i * page;
            crate::hdr_blocks::visit_hdr_blocks(format, self.width, self.height, &blocks, |i, px| {
                let a = page_texels[i];
                words[base + i] = rgb9e5(px);
                if page_mask[i] {
                    let top = a.iter().copied().fold(0.0f32, f32::max);
                    for c in 0..3 {
                        rel.push(if top <= 0.0 {
                            0.0
                        } else {
                            (a[c] - px[c]).abs() / top
                        });
                    }
                }
            })
            .map_err(|e| LightmapError::Payload(format!("encoded blocks do not decode: {e}")))?;
            // One page is the common file: keep the encoder's buffer. Later pages append.
            if data.is_empty() {
                data = blocks;
            } else {
                data.extend(blocks);
            }
        }
        // Mean, p99 and max depend on the ordered values, not on which equal sample was first.
        // `sort_by` is a stable sort and allocates a second copy of the samples.
        rel.sort_unstable_by(f32::total_cmp);
        let report = LightmapEncodeReport {
            encoding,
            bytes: data.len(),
            rgb9e5_bytes,
            texels: rel.len() / 3,
            mean_rel: if rel.is_empty() {
                0.0
            } else {
                (rel.iter().map(|&r| f64::from(r)).sum::<f64>() / rel.len() as f64) as f32
            },
            p99_rel: rel.get(rel.len() * 99 / 100).copied().unwrap_or(0.0),
            max_rel: rel.last().copied().unwrap_or(0.0),
        };
        self.texels = words;
        self.blocks = Some(LightmapBlocks { encoding, data });
        Ok(report)
    }

    /// The hash of the source mesh entry `entry` was baked from ([`lightmap_source_hash`]); `None`
    /// when the bake recorded none (an older file, a bake with no source mesh at hand).
    #[must_use]
    pub fn source_hash(&self, entry: usize) -> Option<u64> {
        self.source_hashes.get(entry).copied().filter(|h| *h != 0)
    }

    /// The cluster blob baked for entry `entry` ([`Self::meshlets`]), when it has one.
    #[must_use]
    pub fn entry_meshlets(&self, entry: usize) -> Option<&[u8]> {
        self.meshlets
            .get(entry)
            .map(Vec::as_slice)
            .filter(|blob| !blob.is_empty())
    }

    /// Entries with clusters baked for their unwrap.
    #[must_use]
    pub fn meshlet_entries(&self) -> usize {
        self.meshlets.iter().filter(|blob| !blob.is_empty()).count()
    }

    /// Writes the lightmap to `path` atomically (a sibling temporary file, then a rename).
    ///
    /// # Errors
    ///
    /// [`Self::to_bytes`] or file IO failed.
    pub fn write(&self, path: &std::path::Path) -> Result<(), LightmapError> {
        let bytes = self.to_bytes()?;
        // pid plus a process-wide counter: two threads writing the same lightmap must not
        // share (and interleave into) one temporary file.
        static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(format!(".tmp{}-{n}", std::process::id()));
        let tmp = std::path::PathBuf::from(tmp);
        if let Err(e) = std::fs::write(&tmp, bytes) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// Reads a lightmap from `path`.
    ///
    /// # Errors
    ///
    /// File IO failed or see [`Self::from_bytes`].
    pub fn read(path: &std::path::Path) -> Result<Self, LightmapError> {
        Self::from_bytes(&std::fs::read(path)?)
    }
}

/// The format version of a lightmap file's header (no other validation).
///
/// # Errors
///
/// [`LightmapError::BadMagic`] when the bytes are not a lightmap.
pub fn lightmap_version(bytes: &[u8]) -> Result<u32, LightmapError> {
    if bytes.len() < 12 || bytes[0..4] != LIGHTMAP_MAGIC {
        return Err(LightmapError::BadMagic);
    }
    Ok(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]))
}

/// Serde field lists of the lightmap header and every struct it holds, for `cargo xtask
/// check-formats` (`docs/specs/12-assets.md` § 4a).
#[must_use]
pub fn lightmap_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names};
    join_field_lists(&[
        serde_field_names::<LightmapHeader>(),
        serde_field_names::<LightmapEntry>(),
        serde_field_names::<LightmapEntryKey>(),
        serde_field_names::<LightmapSeams>(),
        serde_field_names::<LightmapBakeInfo>(),
        serde_field_names::<SectionRef>(),
        serde_field_names::<LightmapDenoiseInfo>(),
    ])
}

/// The versions a `.flmap` migration step leaves (`from` side): versions 1 to 6, all to the
/// current one ([`migrate_lightmap_file`]).
#[must_use]
pub fn lightmap_migration_sources() -> Vec<u32> {
    vec![1, 2, 3, 4, 5, 6]
}

/// Rewrites an older `.flmap` at [`LIGHTMAP_FORMAT_VERSION`] (`regia upgrade-project --write`).
/// Version 1 gains the empty direction section, versions 1 and 2 gain entries with no key and no
/// seam figures (a re-bake from a scene fills them), and versions 1 to 3 gain the empty cluster
/// section (a re-bake with `--meshlets` fills it), and versions 1 to 4 gain an empty denoise record
/// (their texels stay RGB9E5), and versions 1 to 5 gain no source hashes (a re-bake fills them),
/// and versions 1 to 6 become one page (every entry on page 0, ADR-3300):
/// texels, coverage, geometry and directions are copied unchanged, so the payload hash stays the same.
///
/// # Errors
///
/// The file does not read ([`Lightmap::from_bytes`]; the message starts with its agent code), or
/// it is already at the current version.
pub fn migrate_lightmap_file(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let found = lightmap_version(bytes).map_err(|e| format!("{}: {e}", e.code()))?;
    if found >= LIGHTMAP_FORMAT_VERSION {
        return Err(format!(
            "lightmap is version {found}; only older files migrate to {LIGHTMAP_FORMAT_VERSION}"
        ));
    }
    let map = Lightmap::from_bytes(bytes).map_err(|e| format!("{}: {e}", e.code()))?;
    map.to_bytes().map_err(|e| format!("{}: {e}", e.code()))
}

/// The hash a bake records for the source mesh of an entry and a receiver compares its mesh with
/// (version 6, ADR-2656): blake3 over the vertex count, the mesh-space positions and the triangle
/// list, cut to 64 bits. Never 0, which means "no hash" in [`Lightmap::source_hashes`]. A negative
/// zero hashes as zero.
#[must_use]
pub fn lightmap_source_hash(positions: impl ExactSizeIterator<Item = [f32; 3]>, indices: &[u32]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(positions.len() as u64).to_le_bytes());
    for p in positions {
        for c in p {
            hasher.update(&(c + 0.0).to_bits().to_le_bytes());
        }
    }
    hasher.update(&(indices.len() as u64).to_le_bytes());
    for i in indices {
        hasher.update(&i.to_le_bytes());
    }
    let h = hasher.finalize();
    let h = h.as_bytes();
    u64::from_le_bytes([h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]]).max(1)
}

/// Decodes a direction section of `pages` pages, each `encoding`'s section for `width × height`.
fn decode_direction_pages(
    encoding: LightmapDirectionEncoding,
    width: u32,
    height: u32,
    pages: usize,
    data: &[u8],
) -> Result<Vec<u32>, LightmapError> {
    let per_page = encoding.section_bytes(width, height);
    let Some(need) = per_page.checked_mul(pages) else {
        return Err(LightmapError::Payload(format!(
            "direction section for {pages} page(s) of {width}×{height} does not fit"
        )));
    };
    if per_page == 0 || data.len() != need {
        return Err(LightmapError::Payload(format!(
            "direction section holds {} bytes for {pages} page(s) of {width}×{height}",
            data.len()
        )));
    }
    let Some(texels) = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(pages))
    else {
        return Err(LightmapError::Payload(format!(
            "direction section for {pages} page(s) of {width}×{height} does not fit"
        )));
    };
    let mut out = Vec::with_capacity(texels);
    for page in data.chunks(per_page) {
        encoding.decode_into(width, height, page, &mut out)?;
    }
    Ok(out)
}

/// The cluster section cut into one blob per entry.
fn split_meshlets(section: &[u8], sizes: &[u64]) -> Result<Vec<Vec<u8>>, LightmapError> {
    let mut blobs = Vec::with_capacity(sizes.len());
    let mut rest = section;
    for &size in sizes {
        let (blob, tail) = usize::try_from(size)
            .ok()
            .and_then(|n| rest.split_at_checked(n))
            .ok_or_else(|| LightmapError::Payload("a cluster blob runs past its section".into()))?;
        blobs.push(blob.to_vec());
        rest = tail;
    }
    if !rest.is_empty() {
        return Err(LightmapError::Payload(format!(
            "{} bytes of the cluster section belong to no entry",
            rest.len()
        )));
    }
    Ok(blobs)
}

fn rects_overlap(a: [u32; 4], b: [u32; 4]) -> bool {
    a[0] < b[0] + b[2] && b[0] < a[0] + a[2] && a[1] < b[1] + b[3] && b[1] < a[1] + a[3]
}

fn align16(n: usize) -> usize {
    n.div_ceil(16) * 16
}

/// Power-of-two encode multiplier that maps `peak` to at most 32768 (RGB9E5 tops out at 65408).
fn exposure_for(peak: f32) -> f32 {
    if !(peak.is_finite() && peak > 0.0) {
        return 1.0;
    }
    // Bit-field floor(log2): libm `log2` is not correctly rounded, so hosts could disagree.
    crate::baked_gi::exp2_int(crate::baked_gi::floor_log2(32768.0 / peak).clamp(-60, 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second lookup of a known name and of a missing name must not walk `entries` again.
    #[test]
    fn asset1_perf_entry_lookup_is_not_scanned_twice() {
        let map = fixture();
        reset_lightmap_entry_counts();
        assert_eq!(map.entry("b"), Some(1));
        let first = lightmap_entry_scans();
        assert!(first >= 2, "the first lookup walked {first} entries");
        reset_lightmap_entry_counts();
        assert_eq!(map.entry("a"), Some(0));
        assert_eq!(map.entry("missing"), None);
        assert_eq!(
            lightmap_entry_scans(),
            0,
            "second lookups compared {} entries",
            lightmap_entry_scans()
        );
    }

    /// An in-place rename, a key written after the first lookup, and a replaced entry list are
    /// visible to the next lookup.
    #[test]
    fn entry_index_follows_in_place_edits_and_a_replaced_vec() {
        let mut map = fixture();
        assert_eq!(map.entry("a"), Some(0));
        map.entries[0].name = "renamed".into();
        assert_eq!(map.entry("renamed"), Some(0));
        assert_eq!(map.entry("a"), None);
        let key = LightmapEntryKey {
            entity: regia_core::id::Guid::from_u128(7),
            part: 1,
        };
        map.entries[0].key = Some(key);
        assert_eq!(map.entry_for_key(key), Some(0));
        let mut only = map.entries[0].clone();
        only.name = "only".into();
        map.entries = vec![only];
        assert_eq!(map.entry("only"), Some(0));
        assert_eq!(map.entry("renamed"), None);
        assert_eq!(map.entry_for_key(key), Some(0));
    }

    /// corr-asset: a name edited in place keeps its allocation and length; the index built before
    /// the edit must not call the new name missing.
    #[test]
    fn corr_asset_entry_renamed_in_place_is_found() {
        let mut map = fixture();
        assert_eq!(map.entry("a"), Some(0));
        assert_eq!(map.entry("A"), None);
        map.entries[0].name.make_ascii_uppercase();
        assert_eq!(map.entry("A"), Some(0), "an in-place rename is found");
        assert_eq!(map.entry("a"), None);
    }

    #[test]
    fn exposure_is_exact_around_powers_of_two() {
        assert_eq!(exposure_for(1.0), 32768.0);
        assert_eq!(exposure_for(2.0), 16384.0);
        // 32768/peak just below 2^15 floors to 2^14, whatever libm log2 says.
        assert_eq!(exposure_for(f32::from_bits(1.0f32.to_bits() + 1)), 16384.0);
        assert_eq!(exposure_for(f32::NAN), 1.0);
        assert_eq!(exposure_for(0.0), 1.0);
        assert_eq!(exposure_for(1e-30), 2.0f32.powi(60));
    }

    /// A 8×4 atlas with two entries side by side: entry `a` is one quad (4 vertices, 2
    /// triangles) on the left half, `b` the same on the right half with a seam-split vertex.
    fn fixture() -> Lightmap {
        let (w, h) = (8u32, 4u32);
        let mut irr = vec![[0.0f32; 3]; (w * h) as usize];
        let mut cov = vec![Coverage::EMPTY; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) as usize;
                if x == 0 || x == 7 {
                    continue;
                }
                irr[i] = [100.0 + x as f32, 50.0, 25.0 * y as f32];
                cov[i] = if x == 3 || x == 4 {
                    Coverage::DILATED
                } else {
                    Coverage::BAKED
                };
            }
        }
        let quad = MeshUnwrap {
            xref: vec![0, 1, 2, 3],
            uv: vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
            indices: vec![0, 1, 2, 0, 2, 3],
        };
        let mut split = quad.clone();
        split.xref.push(2);
        split.uv.push([0.5, 0.5]);
        split.indices[2] = 4;
        let entry = |name: &str, x: u32| LightmapEntry {
            name: name.into(),
            source_vertices: 4,
            rect: [x, 0, 4, 4],
            scale_offset: [0.5, 1.0, x as f32 / 8.0, 0.0],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: 1,
            area_m2: 1.0,
            texels_per_metre: 4.0,
            key: None,
            seams: None,
            page: 0,
        };
        let layout = LightmapLayout {
            width: w,
            height: h,
            density: 4.0,
            padding: 1,
            entries: vec![entry("a", 0), entry("b", 4)],
            unwraps: vec![quad, split],
        };
        Lightmap::from_irradiance(
            layout,
            &irr,
            cov,
            LightmapBakeInfo {
                contents: LightmapContents::SKY | LightmapContents::SUN_BOUNCE,
                samples: 64,
                bounces: 3,
                seed: 7,
                sun_toward: [0.0, 1.0, 0.0],
                sun_illuminance: 1000.0,
                filtered: true,
            },
        )
        .expect("fixture is valid")
    }

    #[test]
    fn round_trips_byte_for_byte() {
        let map = fixture();
        assert_eq!(map.entries[1].first_vertex, 4);
        assert_eq!(map.entries[1].vertex_count, 5);
        assert_eq!(map.entries[1].first_index, 6);
        let bytes = map.to_bytes().expect("encode");
        assert_eq!(&bytes[0..4], b"FLMP");
        assert_eq!(
            lightmap_version(&bytes).expect("version"),
            LIGHTMAP_FORMAT_VERSION
        );
        assert!(bytes.len().is_multiple_of(16));
        let back = Lightmap::from_bytes(&bytes).expect("decode");
        assert_eq!(back, map);
        assert_eq!(back.to_bytes().expect("re-encode"), bytes);
        // Stale offsets are settled in the file without touching the map.
        let mut stale = map.clone();
        stale.entries[1].first_vertex = 77;
        stale.entries[0].index_count = 1;
        assert_eq!(stale.to_bytes().expect("encode stale"), bytes);
        assert_eq!(
            stale.entries[1].first_vertex, 77,
            "to_bytes does not change the map"
        );
        let header = Lightmap::read_header(&bytes).expect("header");
        assert_eq!(header.entries.len(), 2);
        assert_eq!(header.texel_encoding, TEXEL_ENCODING_RGB9E5);
    }

    #[test]
    fn decodes_irradiance_within_rgb9e5_precision() {
        let map = fixture();
        let got = map.irradiance_at(2, 1);
        let want = [102.0f32, 50.0, 25.0];
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() <= w * 0.01 + 1e-3, "{got:?} vs {want:?}");
        }
        assert_eq!(map.irradiance_at(99, 0), [0.0; 3]);
        assert!(
            map.exposure.log2().fract().abs() < 1e-6,
            "exposure is a power of two"
        );
    }

    #[test]
    fn bilinear_sample_ignores_empty_texels() {
        let map = fixture();
        // Halfway between empty column 0 and baked column 1: only column 1 counts.
        let uv = [1.0 / 8.0, 1.5 / 4.0];
        let got = map.sample(uv);
        assert_eq!(
            got,
            map.irradiance_at(1, 1),
            "no black pulled in from the empty column"
        );
        // Inside an empty region only: black.
        let mut empty = map.clone();
        empty.coverage.fill(Coverage::EMPTY);
        assert_eq!(empty.sample([0.5, 0.5]), [0.0; 3]);
        // A texel centre returns that texel.
        let centre = map.sample([2.5 / 8.0, 1.5 / 4.0]);
        assert_eq!(centre, map.irradiance_at(2, 1));
        // Entry-local lookups go through scale_offset.
        let local = map.sample_entry(1, [0.5, 0.5]);
        let atlas = map.sample([0.75, 0.5]);
        assert_eq!(local, atlas);
        assert_eq!(map.coverage_counts(), [8, 16, 8]);
        assert!((map.utilization() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn corrupt_truncated_and_newer_files_are_refused_with_codes() {
        let bytes = fixture().to_bytes().expect("encode");
        let mut corrupt = bytes.clone();
        let last = corrupt.len() - 20;
        corrupt[last] ^= 0x5a;
        let e = Lightmap::from_bytes(&corrupt).expect_err("corrupt");
        assert_eq!(e.code(), "lightmap_payload");

        let e = Lightmap::from_bytes(&bytes[..bytes.len() / 2]).expect_err("truncated");
        assert_eq!(e.code(), "lightmap_payload");

        let mut newer = bytes.clone();
        newer[4..8].copy_from_slice(&(LIGHTMAP_FORMAT_VERSION + 1).to_le_bytes());
        let e = Lightmap::from_bytes(&newer).expect_err("newer");
        assert_eq!(e.code(), "lightmap_version");
        assert!(e.to_string().contains("not supported"), "{e}");

        let e = Lightmap::from_bytes(b"FGIV\x01\0\0\0\0\0\0\0").expect_err("other format");
        assert_eq!(e.code(), "lightmap_bad_magic");
        assert_eq!(
            Lightmap::from_bytes(&[]).expect_err("empty").code(),
            "lightmap_bad_magic"
        );
    }

    #[test]
    fn a_huge_atlas_size_is_refused_before_any_size_product() {
        // `u32::MAX` × `u32::MAX` with a matching hash (empty sections, the empty hash) used to
        // overflow `n * 4` before the size cap (ADR-1660).
        let mut header = Lightmap::read_header(&fixture().to_bytes().expect("encode")).expect("header");
        header.width = u32::MAX;
        header.height = u32::MAX;
        header.sections = [SectionRef::default(); LIGHTMAP_SECTIONS];
        header.meshlet_section = SectionRef::default();
        header.payload_hash = *blake3::Hasher::new().finalize().as_bytes();
        let header_bytes = postcard::to_allocvec(&header).expect("header encode");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&LIGHTMAP_MAGIC);
        bytes.extend_from_slice(&LIGHTMAP_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&header_bytes);
        let e = Lightmap::from_bytes(&bytes).expect_err("huge atlas");
        assert_eq!(e.code(), "lightmap_layout", "{e}");
        // One side over the cap is refused the same way.
        header.width = 4;
        header.height = MAX_LIGHTMAP_SIZE + 1;
        let header_bytes = postcard::to_allocvec(&header).expect("header encode");
        bytes.truncate(8);
        bytes.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&header_bytes);
        assert_eq!(
            Lightmap::from_bytes(&bytes).expect_err("tall atlas").code(),
            "lightmap_layout"
        );
    }

    #[test]
    fn huge_entry_vertex_counts_are_refused_without_panic() {
        // Audit (asset1): on a 32-bit `usize` the entry-count sums and the
        // `vertex_count * UNWRAP_VERTEX_BYTES` product wrapped to zero, the geometry length check
        // passed against the small real section, and the records slice panicked. The sums run in
        // `u64` and the slices are taken with `get` now. The file keeps every real section so the
        // forged header passes the hash and reaches the geometry check.
        let orig = fixture().to_bytes().expect("encode");
        let orig_header = Lightmap::read_header(&orig).expect("header");
        let bodies: [&[u8]; LIGHTMAP_SECTIONS] = core::array::from_fn(|k| {
            let s = orig_header.sections[k];
            &orig[s.offset as usize..(s.offset + s.len) as usize]
        });
        let clusters = &orig[orig_header.meshlet_section.offset as usize
            ..(orig_header.meshlet_section.offset + orig_header.meshlet_section.len) as usize];
        let mut header = orig_header;
        header.entries[0].vertex_count = 1 << 30;
        header.sections = [SectionRef::default(); LIGHTMAP_SECTIONS];
        header.meshlet_section = SectionRef::default();
        let mut hasher = blake3::Hasher::new();
        for b in bodies {
            hasher.update(b);
        }
        hasher.update(clusters);
        header.payload_hash = *hasher.finalize().as_bytes();
        let mut header_bytes = postcard::to_allocvec(&header).expect("header encode");
        for _ in 0..4 {
            let mut at = align16(12 + header_bytes.len()) as u64;
            for (s, b) in header.sections.iter_mut().zip(bodies) {
                *s = SectionRef {
                    offset: at,
                    len: b.len() as u64,
                };
                at = align16((at + b.len() as u64) as usize) as u64;
            }
            header.meshlet_section = SectionRef {
                offset: at,
                len: clusters.len() as u64,
            };
            let next = postcard::to_allocvec(&header).expect("header encode");
            let settled = next.len() == header_bytes.len();
            header_bytes = next;
            if settled {
                break;
            }
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&LIGHTMAP_MAGIC);
        bytes.extend_from_slice(&LIGHTMAP_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&header_bytes);
        for (s, b) in header.sections.iter().zip(bodies) {
            bytes.resize(s.offset as usize, 0);
            bytes.extend_from_slice(b);
        }
        bytes.resize(header.meshlet_section.offset as usize, 0);
        bytes.extend_from_slice(clusters);
        let e = Lightmap::from_bytes(&bytes).expect_err("a huge entry vertex count");
        assert_eq!(e.code(), "lightmap_payload", "{e}");
    }

    #[test]
    fn a_section_offset_past_32_bits_is_refused() {
        // Audit (asset1): on a 32-bit `usize` a section offset of `1 << 32` truncated to 0 in
        // `as usize`, so the hash was compared against the wrong bytes. The offsets convert with
        // `usize::try_from` now, like `.fgiv` already does.
        let orig = fixture().to_bytes().expect("encode");
        let mut header = Lightmap::read_header(&orig).expect("header");
        for s in &mut header.sections {
            s.offset = 1 << 32;
        }
        header.meshlet_section.offset = 1 << 32;
        let header_bytes = postcard::to_allocvec(&header).expect("header encode");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&LIGHTMAP_MAGIC);
        bytes.extend_from_slice(&LIGHTMAP_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&header_bytes);
        let e = Lightmap::from_bytes(&bytes).expect_err("a 33-bit section offset");
        assert_eq!(e.code(), "lightmap_payload", "{e}");
    }

    #[test]
    fn a_subnormal_exposure_is_refused_since_its_decode_scale_is_infinite() {
        let mut map = fixture();
        map.exposure = f32::from_bits(1);
        assert!((1.0 / map.exposure).is_infinite());
        let e = map.validate().expect_err("subnormal exposure");
        assert_eq!(e.code(), "lightmap_layout");
        // The smallest normal exposure still has a finite reciprocal and passes.
        map.exposure = f32::MIN_POSITIVE;
        assert!(map.validate().is_ok());
    }

    #[test]
    fn validate_names_overlaps_bad_uvs_and_bad_indices() {
        let mut map = fixture();
        map.entries[1].rect = [2, 0, 4, 4];
        let e = map.validate().expect_err("overlap");
        assert!(e.to_string().contains("overlaps"), "{e}");
        assert_eq!(e.code(), "lightmap_layout");

        let mut map = fixture();
        map.unwraps[0].uv[1] = [1.5, 0.0];
        assert!(
            map.validate()
                .expect_err("uv")
                .to_string()
                .contains("outside 0..1")
        );

        let mut map = fixture();
        map.unwraps[0].indices[0] = 40;
        assert!(
            map.validate()
                .expect_err("index")
                .to_string()
                .contains("past the unwrapped")
        );

        let mut map = fixture();
        map.unwraps[0].xref[0] = 4;
        assert!(
            map.validate()
                .expect_err("xref")
                .to_string()
                .contains("past the source")
        );

        let mut map = fixture();
        map.entries[1].name = "a".into();
        assert!(map.validate().expect_err("dup").to_string().contains("duplicate"));

        let mut map = fixture();
        map.entries[0].rect = [6, 0, 4, 4];
        assert!(
            map.validate()
                .expect_err("outside")
                .to_string()
                .contains("outside the atlas")
        );

        let mut map = fixture();
        map.bake.contents |= 64;
        assert!(map.validate().is_err());

        let mut map = fixture();
        map.coverage[0] = 3;
        assert!(map.validate().is_err());
    }

    #[test]
    fn writes_atomically_and_reads_back() {
        let dir = regia_test_utils::unique_temp_dir("regia_asset_lightmap_write");
        let path = dir.join("map.flmap");
        let map = fixture();
        map.write(&path).expect("write");
        let names: Vec<_> = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temporary file left: {names:?}");
        assert_eq!(Lightmap::read(&path).expect("read"), map);
        let e = Lightmap::read(&dir.join("missing.flmap")).expect_err("missing");
        assert_eq!(e.code(), "lightmap_io");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_lists_every_header_struct_and_contents_names() {
        let f = lightmap_schema_fingerprint();
        for field in [
            "texel_encoding",
            "scale_offset",
            "sun_illuminance",
            "payload_hash",
            "offset",
        ] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
        assert!(f.contains("direction_encoding"), "{f}");
        // Version 3's entry key and seam figures are part of the frozen shape.
        for field in ["entity", "worst_chart", "seams"] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
        // Version 5's denoise record is part of it too.
        for field in ["denoise", "sigma_luminance", "normal_power", "iterations"] {
            assert!(f.contains(field), "{field} missing from {f}");
        }
        // Version 6's source hashes too.
        assert!(f.contains("source_hashes"), "{f}");
        // Version 7's pages (ADR-3300): the header's count and every entry's page.
        assert!(f.contains("pages") && f.contains("page"), "{f}");
        assert_eq!(lightmap_migration_sources(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(
            LightmapContents::names(LightmapContents::SKY | LightmapContents::EMISSIVE),
            ["sky", "emissive"]
        );
    }

    /// A version 1 file written by v0.1.28's encoder (`tests/fixtures/lightmap-v1.flmap`, the
    /// fixture above with both entries a plain quad): it reads as a lightmap without
    /// directions, and the migrator rewrites it at version 2 with the same sections and hash.
    #[test]
    fn version_1_files_read_and_migrate_to_version_2_unchanged() {
        let v1: &[u8] = include_bytes!("../tests/fixtures/lightmap-v1.flmap");
        assert_eq!(lightmap_version(v1).expect("version"), 1);
        let header = Lightmap::read_header(v1).expect("v1 header");
        assert_eq!(header.direction_encoding, DIRECTION_NONE);
        assert_eq!(header.sections[3].len, 0);
        let map = Lightmap::from_bytes(v1).expect("v1 reads");
        assert!(!map.is_directional());
        assert_eq!((map.width, map.height, map.entries.len()), (8, 4, 2));
        assert_eq!(map.bake.seed, 7);
        let mut want = fixture();
        want.unwraps[1] = want.unwraps[0].clone();
        want.assign_geometry_offsets();
        assert_eq!(map.texels, want.texels);
        assert_eq!(map.coverage, want.coverage);
        assert_eq!(map.unwraps, want.unwraps);

        let v2 = migrate_lightmap_file(v1).expect("migrates");
        assert_eq!(lightmap_version(&v2).expect("version"), LIGHTMAP_FORMAT_VERSION);
        let back = Lightmap::from_bytes(&v2).expect("v2 reads");
        assert_eq!(back, map);
        let h2 = Lightmap::read_header(&v2).expect("v2 header");
        assert_eq!(
            h2.payload_hash, header.payload_hash,
            "an empty fourth section keeps the hash"
        );
        for k in 0..3 {
            let (a, b) = (header.sections[k], h2.sections[k]);
            assert_eq!(
                v1[a.offset as usize..(a.offset + a.len) as usize],
                v2[b.offset as usize..(b.offset + b.len) as usize],
                "section {k} is copied unchanged"
            );
        }
        let e = migrate_lightmap_file(&v2).expect_err("already current");
        assert!(e.contains("only older files"), "{e}");
        assert!(
            migrate_lightmap_file(b"nope")
                .expect_err("not a lightmap")
                .starts_with("lightmap_bad_magic")
        );
    }

    /// A version 3 file written by the version 3 encoder (`tests/fixtures/lightmap-v3.flmap`:
    /// the version 2 fixture with a key and seam figures on its first entry): it reads with no
    /// clusters, and the migrator rewrites it at the current version with the same sections and
    /// hash.
    #[test]
    fn version_3_files_read_and_migrate_to_version_4_unchanged() {
        let v3: &[u8] = include_bytes!("../tests/fixtures/lightmap-v3.flmap");
        assert_eq!(lightmap_version(v3).expect("version"), 3);
        let header = Lightmap::read_header(v3).expect("v3 header");
        assert_eq!(header.meshlet_section, SectionRef::default());
        assert!(header.meshlet_bytes.is_empty());
        assert_eq!(header.entries[0].key.map(|k| k.part), Some(1));
        assert_eq!(header.entries[0].seams.map(|s| s.edges), Some(3));
        let map = Lightmap::from_bytes(v3).expect("v3 reads");
        assert!(map.meshlets.is_empty());
        assert_eq!(map.meshlet_entries(), 0);
        assert_eq!(map.entry_meshlets(0), None);

        let v4 = migrate_lightmap_file(v3).expect("migrates");
        assert_eq!(lightmap_version(&v4).expect("version"), LIGHTMAP_FORMAT_VERSION);
        assert_eq!(Lightmap::from_bytes(&v4).expect("v4 reads"), map);
        let h4 = Lightmap::read_header(&v4).expect("v4 header");
        assert_eq!(h4.payload_hash, header.payload_hash, "the payload is untouched");
        for k in 0..LIGHTMAP_SECTIONS {
            let (a, b) = (header.sections[k], h4.sections[k]);
            assert_eq!(
                v3[a.offset as usize..(a.offset + a.len) as usize],
                v4[b.offset as usize..(b.offset + b.len) as usize],
                "section {k} is copied unchanged"
            );
        }
        assert_eq!(h4.meshlet_section.len, 0);
    }

    /// A version 5 file written by the version 5 encoder (`tests/fixtures/lightmap-v5.flmap`: the
    /// version 4 fixture migrated by that build): it reads with no source hash, and the migrator
    /// rewrites it at the current version with the same sections and hash.
    #[test]
    fn version_5_files_read_and_migrate_unchanged() {
        let v5: &[u8] = include_bytes!("../tests/fixtures/lightmap-v5.flmap");
        assert_eq!(lightmap_version(v5).expect("version"), 5);
        let header = Lightmap::read_header(v5).expect("v5 header");
        assert!(header.source_hashes.is_empty());
        let map = Lightmap::from_bytes(v5).expect("v5 reads");
        assert!(map.is_directional());
        assert_eq!(map.entry_meshlets(0), Some(&[1u8, 2, 3, 4, 5][..]));
        assert_eq!(map.source_hash(0), None);
        assert_eq!(
            map.texels,
            fixture().texels,
            "the version 5 texels are the fixture's"
        );

        let v6 = migrate_lightmap_file(v5).expect("migrates");
        assert_eq!(lightmap_version(&v6).expect("version"), LIGHTMAP_FORMAT_VERSION);
        assert_eq!(Lightmap::from_bytes(&v6).expect("v6 reads"), map);
        let h6 = Lightmap::read_header(&v6).expect("v6 header");
        assert_eq!(h6.payload_hash, header.payload_hash, "the payload is untouched");
        for k in 0..LIGHTMAP_SECTIONS {
            let (a, b) = (header.sections[k], h6.sections[k]);
            assert_eq!(
                v5[a.offset as usize..(a.offset + a.len) as usize],
                v6[b.offset as usize..(b.offset + b.len) as usize],
                "section {k} is copied unchanged"
            );
        }
    }

    /// Version 6 keeps a bake's source hashes; a list that does not fit the entries is refused.
    #[test]
    fn source_hashes_round_trip_and_must_fit_the_entries() {
        let mut map = fixture();
        assert_eq!(map.source_hash(0), None, "a bake without hashes");
        map.source_hashes = vec![7, 0];
        let back = Lightmap::from_bytes(&map.to_bytes().expect("writes")).expect("reads");
        assert_eq!(back.source_hash(0), Some(7));
        assert_eq!(back.source_hash(1), None, "0 means no hash");
        assert_eq!(back.source_hash(2), None, "past the list");
        map.source_hashes = vec![7];
        assert_eq!(map.validate().expect_err("one short").code(), "lightmap_layout");
    }

    /// The source hash moves with any position or index, ignores the sign of a zero, and is never 0.
    #[test]
    fn the_source_hash_follows_the_mesh() {
        let positions = [[0.0_f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let hash = |p: &[[f32; 3]], i: &[u32]| lightmap_source_hash(p.iter().copied(), i);
        let base = hash(&positions, &[0, 1, 2]);
        assert_ne!(base, 0);
        assert_eq!(base, hash(&positions, &[0, 1, 2]), "stable");
        let mut moved = positions;
        moved[2][1] = 1.001;
        assert_ne!(base, hash(&moved, &[0, 1, 2]));
        assert_ne!(base, hash(&positions, &[0, 2, 1]));
        let mut negative = positions;
        negative[0][0] = -0.0;
        assert_eq!(base, hash(&negative, &[0, 1, 2]));
    }

    /// A version 4 file written by the version 4 encoder (`tests/fixtures/lightmap-v4.flmap`: the
    /// fixture with directions and a cluster blob on entry `a`): it reads with RGB9E5 texels and
    /// no denoise record, and the migrator rewrites it at version 5 with the same sections and
    /// hash.
    #[test]
    fn version_4_files_read_and_migrate_to_version_5_unchanged() {
        let v4: &[u8] = include_bytes!("../tests/fixtures/lightmap-v4.flmap");
        assert_eq!(lightmap_version(v4).expect("version"), 4);
        let header = Lightmap::read_header(v4).expect("v4 header");
        assert_eq!(header.texel_encoding, TEXEL_ENCODING_RGB9E5);
        assert_eq!(header.denoise, None);
        let map = Lightmap::from_bytes(v4).expect("v4 reads");
        assert_eq!(map.texel_encoding(), LightmapTexelEncoding::Rgb9e5);
        assert!(map.is_directional());
        assert_eq!(map.entry_meshlets(0), Some(&[1u8, 2, 3, 4, 5][..]));
        assert_eq!(map.denoise, None);
        let want = fixture();
        assert_eq!(map.texels, want.texels, "the version 4 texels are the fixture's");

        let v5 = migrate_lightmap_file(v4).expect("migrates");
        assert_eq!(lightmap_version(&v5).expect("version"), LIGHTMAP_FORMAT_VERSION);
        assert_eq!(Lightmap::from_bytes(&v5).expect("v5 reads"), map);
        let h5 = Lightmap::read_header(&v5).expect("v5 header");
        assert_eq!(h5.payload_hash, header.payload_hash, "the payload is untouched");
        for k in 0..LIGHTMAP_SECTIONS {
            let (a, b) = (header.sections[k], h5.sections[k]);
            assert_eq!(
                v4[a.offset as usize..(a.offset + a.len) as usize],
                v5[b.offset as usize..(b.offset + b.len) as usize],
                "section {k} is copied unchanged"
            );
        }
        assert!(lightmap_migration_sources().contains(&4));
    }

    /// Both block encodings: the file stores the blocks (one byte a texel instead of four), the
    /// texels decode within the documented error of the RGB9E5 ones, the file round-trips byte for
    /// byte, and back to RGB9E5 drops the blocks.
    #[test]
    fn block_encodings_round_trip_within_the_documented_error() {
        use crate::hdr_blocks::{ASTC_HDR_MAX_REL_ERROR, BC6H_MAX_REL_ERROR};
        // The fixture's layout with smooth light (the documented error's condition: a block's
        // texels within a factor of two).
        let mut plain = fixture();
        for (k, t) in plain.texels.iter_mut().enumerate() {
            let (x, y) = ((k % 8) as f32, (k / 8) as f32);
            if plain.coverage[k] != Coverage::EMPTY {
                *t = rgb9e5([100.0 + x, 50.0 + 2.0 * y, 30.0 + x + y].map(|v| v * plain.exposure));
            }
        }
        for (encoding, code, bound) in [
            (
                LightmapTexelEncoding::Bc6h,
                TEXEL_ENCODING_BC6H,
                BC6H_MAX_REL_ERROR,
            ),
            (
                LightmapTexelEncoding::AstcHdr,
                TEXEL_ENCODING_ASTC_HDR,
                ASTC_HDR_MAX_REL_ERROR,
            ),
        ] {
            let mut map = plain.clone();
            let report = map.encode_texels(encoding).expect("encodes");
            assert_eq!(report.encoding, encoding);
            assert_eq!((report.bytes, report.rgb9e5_bytes), (2 * 16, 8 * 4 * 4));
            assert_eq!(report.texels, 24, "baked and dilated texels");
            assert!(
                report.max_rel <= bound && report.mean_rel <= bound / 4.0,
                "{report:?}"
            );
            assert_eq!(map.texel_encoding(), encoding);
            for (k, (a, b)) in plain.texels.iter().zip(&map.texels).enumerate() {
                if plain.coverage[k] == Coverage::EMPTY {
                    continue;
                }
                let (a, b) = (from_rgb9e5(*a), from_rgb9e5(*b));
                for c in 0..3 {
                    assert!(
                        (a[c] - b[c]).abs() <= a[0].max(a[1]).max(a[2]) * bound * 1.01,
                        "{k}: {a:?} {b:?}"
                    );
                }
            }
            let bytes = map.to_bytes().expect("writes");
            let header = Lightmap::read_header(&bytes).expect("header");
            assert_eq!(header.texel_encoding, code);
            assert_eq!(header.sections[0].len, 32);
            let back = Lightmap::from_bytes(&bytes).expect("reads");
            assert_eq!(back, map, "{encoding:?}");
            assert_eq!(back.to_bytes().expect("rewrites"), bytes, "byte for byte");
            // The CPU readers see the decoded blocks.
            assert_eq!(back.irradiance_at(2, 1), map.irradiance_at(2, 1));

            let mut flat = back.clone();
            let r = flat.encode_texels(LightmapTexelEncoding::Rgb9e5).expect("rgb9e5");
            assert_eq!(r.bytes, 8 * 4 * 4);
            assert_eq!(flat.texel_encoding(), LightmapTexelEncoding::Rgb9e5);
            assert_eq!(
                flat.texels, back.texels,
                "dropping the blocks keeps the decoded texels"
            );
        }
        for e in LightmapTexelEncoding::ALL {
            assert_eq!(LightmapTexelEncoding::from_name(e.name()), Some(e));
            assert_eq!(LightmapTexelEncoding::from_code(e.code()), Some(e));
        }
        assert_eq!(LightmapTexelEncoding::from_code(4), None);
    }

    /// A damaged block section, a block count that does not match the atlas, a block encoding in
    /// a file that claims an older version, and blocks the decoder does not read are all refused.
    #[test]
    fn bad_block_sections_are_refused_with_codes() {
        let mut map = fixture();
        map.encode_texels(LightmapTexelEncoding::Bc6h).expect("encodes");
        let good = map.to_bytes().expect("writes");
        // An older version cannot carry blocks.
        let mut old = good.clone();
        old[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert!(Lightmap::from_bytes(&old).is_err());
        // Foreign blocks (an unknown mode) with a matching hash.
        let mut foreign = map.clone();
        if let Some(b) = foreign.blocks.as_mut() {
            b.data.fill(0xFF);
        }
        let bytes = foreign.to_bytes().expect("writes what it is given");
        let e = Lightmap::from_bytes(&bytes).expect_err("foreign blocks");
        assert_eq!(e.code(), "lightmap_payload");
        // Too few blocks for the atlas.
        let mut short = map.clone();
        if let Some(b) = short.blocks.as_mut() {
            b.data.truncate(16);
        }
        assert_eq!(short.to_bytes().expect_err("short").code(), "lightmap_layout");
        // A damaged byte.
        let mut damaged = good.clone();
        let at = Lightmap::read_header(&good).expect("header").sections[0].offset as usize;
        damaged[at] ^= 1;
        assert_eq!(
            Lightmap::from_bytes(&damaged).expect_err("hash").code(),
            "lightmap_payload"
        );
    }

    /// The denoise record round-trips, and an out-of-range one is refused.
    #[test]
    fn the_denoise_record_round_trips_and_is_checked() {
        let mut map = fixture();
        map.denoise = Some(LightmapDenoiseInfo::default());
        let bytes = map.to_bytes().expect("writes");
        assert_eq!(
            Lightmap::read_header(&bytes).expect("header").denoise,
            Some(LightmapDenoiseInfo::default())
        );
        assert_eq!(Lightmap::from_bytes(&bytes).expect("reads"), map);
        for bad in [
            LightmapDenoiseInfo {
                iterations: 0,
                ..LightmapDenoiseInfo::default()
            },
            LightmapDenoiseInfo {
                iterations: MAX_DENOISE_ITERATIONS + 1,
                ..LightmapDenoiseInfo::default()
            },
            LightmapDenoiseInfo {
                sigma_luminance: 0.0,
                ..LightmapDenoiseInfo::default()
            },
            LightmapDenoiseInfo {
                normal_power: f32::NAN,
                ..LightmapDenoiseInfo::default()
            },
            LightmapDenoiseInfo {
                sigma_position: -1.0,
                ..LightmapDenoiseInfo::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
            map.denoise = Some(bad);
            assert_eq!(map.to_bytes().expect_err("bad denoise").code(), "lightmap_layout");
        }
    }

    /// Cluster blobs come back per entry, an entry may have none, a file without any has no bytes
    /// for them, and a header that cuts the section wrongly or a damaged blob is refused.
    #[test]
    fn cluster_blobs_round_trip_per_entry_and_bad_sizes_are_refused() {
        let plain = fixture();
        let plain_bytes = plain.to_bytes().expect("encode");
        let mut with = plain.clone();
        with.meshlets = vec![b"first blob".to_vec(), Vec::new()];
        let bytes = with.to_bytes().expect("encode");
        let back = Lightmap::from_bytes(&bytes).expect("reads");
        assert_eq!(back, with);
        assert_eq!(back.entry_meshlets(0), Some(&b"first blob"[..]));
        assert_eq!(back.entry_meshlets(1), None, "an entry without clusters");
        assert_eq!(back.entry_meshlets(2), None, "no such entry");
        assert_eq!(back.meshlet_entries(), 1);
        let header = Lightmap::read_header(&bytes).expect("header");
        assert_eq!(header.meshlet_bytes, [10, 0]);
        assert_eq!(header.meshlet_section.len, 10);
        assert!(header.meshlet_section.offset.is_multiple_of(16));

        let none = Lightmap::read_header(&plain_bytes).expect("header");
        assert_eq!(none.meshlet_section.len, 0, "clusters are opt-in");
        assert!(none.meshlet_bytes.is_empty());
        assert!(plain_bytes.len() < bytes.len());

        // One blob too few for the entries.
        let mut bad = plain.clone();
        bad.meshlets = vec![b"lonely".to_vec()];
        assert_eq!(bad.to_bytes().expect_err("short list").code(), "lightmap_layout");

        // A flipped blob byte fails the hash.
        let mut flipped = bytes.clone();
        flipped[header.meshlet_section.offset as usize] ^= 1;
        assert_eq!(
            Lightmap::from_bytes(&flipped).expect_err("hash").code(),
            "lightmap_payload"
        );

        // The cut itself, past the section and short of it.
        assert!(split_meshlets(b"abc", &[4]).is_err());
        assert!(split_meshlets(b"abc", &[2]).is_err());
        assert!(split_meshlets(b"abc", &[u64::MAX]).is_err());
        assert_eq!(
            split_meshlets(b"abc", &[2, 0, 1]).expect("fits"),
            [b"ab".to_vec(), Vec::new(), b"c".to_vec()]
        );
    }

    /// A version 2 file written by v0.2.10's encoder (`tests/fixtures/lightmap-v2.flmap`,
    /// `regia bake-lightmaps quad-box --density 4 --samples 4 --seed 3 --threads 1`): it reads with
    /// no entry key and no seam figures, and the migrator rewrites it at the current version with
    /// the same sections and hash.
    #[test]
    fn version_2_files_read_and_migrate_to_version_3_unchanged() {
        let v2: &[u8] = include_bytes!("../tests/fixtures/lightmap-v2.flmap");
        assert_eq!(lightmap_version(v2).expect("version"), 2);
        let header = Lightmap::read_header(v2).expect("v2 header");
        assert_eq!(header.direction_encoding, DIRECTION_RGBA8);
        assert!(
            header
                .entries
                .iter()
                .all(|e| e.key.is_none() && e.seams.is_none())
        );
        let map = Lightmap::from_bytes(v2).expect("v2 reads");
        assert!(map.is_directional());
        assert_eq!(map.bake.seed, 3);
        assert_eq!(map.bake.samples, 4);
        assert_eq!(map.entries[0].name, "instance_0");

        let v3 = migrate_lightmap_file(v2).expect("migrates");
        assert_eq!(lightmap_version(&v3).expect("version"), LIGHTMAP_FORMAT_VERSION);
        assert_eq!(Lightmap::from_bytes(&v3).expect("v3 reads"), map);
        let h3 = Lightmap::read_header(&v3).expect("v3 header");
        assert_eq!(h3.payload_hash, header.payload_hash, "the payload is untouched");
        for k in 0..LIGHTMAP_SECTIONS {
            let (a, b) = (header.sections[k], h3.sections[k]);
            assert_eq!(
                v2[a.offset as usize..(a.offset + a.len) as usize],
                v3[b.offset as usize..(b.offset + b.len) as usize],
                "section {k} is copied unchanged"
            );
        }
        // A re-migration is refused, and so is a damaged old file.
        assert!(
            migrate_lightmap_file(&v3)
                .expect_err("current")
                .contains("only older files")
        );
        let mut bad = v2.to_vec();
        let last = bad.len() - 20;
        bad[last] ^= 0xff;
        assert!(
            migrate_lightmap_file(&bad)
                .expect_err("damaged")
                .starts_with("lightmap_payload"),
            "the hash still guards an old file"
        );
    }

    /// Version 3's new fields round trip, and an entry is found by the key it was baked for.
    #[test]
    fn entry_keys_and_seam_figures_round_trip() {
        let mut map = fixture();
        let key = LightmapEntryKey {
            entity: "00000000-0000-0000-0000-00000000000b".parse().expect("guid"),
            part: 2,
        };
        map.entries[1].key = Some(key);
        map.entries[1].seams = Some(LightmapSeams {
            edges: 3,
            mean: 0.01,
            p95: 0.02,
            max: 0.04,
            worst_chart: 1,
        });
        let bytes = map.to_bytes().expect("encodes");
        assert_eq!(
            lightmap_version(&bytes).expect("version"),
            LIGHTMAP_FORMAT_VERSION
        );
        let back = Lightmap::from_bytes(&bytes).expect("reads");
        assert_eq!(back, map);
        assert_eq!(back.entry_for_key(key), Some(1));
        assert_eq!(back.entry_for_key(LightmapEntryKey { part: 0, ..key }), None);
        let header = Lightmap::read_header(&bytes).expect("header");
        assert_eq!(header.entries[1].seams.map(|s| s.edges), Some(3));
    }

    #[test]
    fn directional_lightmaps_round_trip_and_bad_sections_are_refused() {
        let mut map = fixture();
        let n = (map.width * map.height) as usize;
        let dirs: Vec<[f32; 4]> = (0..n)
            .map(|i| [0.0, 1.0 - i as f32 / n as f32, i as f32 / n as f32, 0.5])
            .collect();
        map.set_directions(&dirs).expect("one per texel");
        let bytes = map.to_bytes().expect("encode");
        let header = Lightmap::read_header(&bytes).expect("header");
        assert_eq!(header.direction_encoding, DIRECTION_RGBA8);
        assert_eq!(header.sections[3].len, n as u64 * 4);
        let back = Lightmap::from_bytes(&bytes).expect("decode");
        assert_eq!(back, map);
        assert_eq!(back.to_bytes().expect("re-encode"), bytes);
        let d = back.direction_at(3, 2).expect("directional");
        assert!((d[3] - 0.5).abs() <= 1.0 / 255.0, "{d:?}");
        assert_eq!(back.direction_at(8, 0), None);

        let mut short = map.clone();
        short.directions.pop();
        assert_eq!(short.validate().expect_err("short").code(), "lightmap_layout");
        assert_eq!(short.to_bytes().expect_err("short").code(), "lightmap_layout");

        // A header that claims an unknown direction encoding: re-encode the header by hand.
        let mut bad = header.clone();
        bad.direction_encoding = 9;
        let hb = postcard::to_allocvec(&bad).expect("header");
        let old_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        assert_eq!(hb.len(), old_len, "same varint sizes");
        let mut forged = bytes.clone();
        forged[12..12 + hb.len()].copy_from_slice(&hb);
        let e = Lightmap::from_bytes(&forged).expect_err("unknown encoding");
        assert_eq!(e.code(), "lightmap_payload");
        assert!(e.to_string().contains("direction encoding"), "{e}");
    }

    /// Two pages (ADR-3300): entry `b` moves to page 1 at the rectangle `a` holds on page 0 (the two
    /// overlap only across pages); page 1's texels are page 0's doubled.
    fn two_pages() -> Lightmap {
        let one = fixture();
        let mut layout = LightmapLayout {
            width: one.width,
            height: one.height,
            density: one.density,
            padding: one.padding,
            entries: one.entries.clone(),
            unwraps: one.unwraps.clone(),
        };
        layout.entries[1].rect = [0, 0, 4, 4];
        layout.entries[1].scale_offset = [0.5, 1.0, 0.0, 0.0];
        layout.entries[1].page = 1;
        let page0: Vec<[f32; 3]> = (0..one.height)
            .flat_map(|y| (0..one.width).map(move |x| (x, y)))
            .map(|(x, y)| one.irradiance_at(x, y))
            .collect();
        let mut irr = page0.clone();
        irr.extend(page0.iter().map(|c| c.map(|v| v * 2.0)));
        let mut cov = one.coverage.clone();
        cov.extend(one.coverage.iter().copied());
        Lightmap::from_irradiance(layout, &irr, cov, one.bake).expect("two pages")
    }

    /// A paged lightmap writes its page count and every entry's page, reads back equal, and each
    /// entry reads only its own page: the same rectangle and UV on page 1 is page 1's light.
    #[test]
    fn pages_round_trip_and_each_entry_reads_its_own_page() {
        let map = two_pages();
        assert_eq!(map.pages(), 2);
        assert_eq!(map.total_texels(), 2 * map.page_texels());
        assert_eq!(map.texel_index(1, 0, 0), Some(map.page_texels()));
        assert_eq!(map.texel_index(2, 0, 0), None);
        let bytes = map.to_bytes().expect("writes");
        assert_eq!(lightmap_version(&bytes).expect("version"), 7);
        let header = Lightmap::read_header(&bytes).expect("header");
        assert_eq!(header.pages, 2);
        assert_eq!(header.entries[1].page, 1);
        let back = Lightmap::from_bytes(&bytes).expect("reads");
        assert_eq!(back, map);
        for local in [[0.3, 0.4], [0.6, 0.5], [0.45, 0.8]] {
            let (a, b) = (back.sample_entry(0, local), back.sample_entry(1, local));
            for k in 0..3 {
                assert!(
                    (b[k] - 2.0 * a[k]).abs() <= 0.01 * a[k].abs().max(1.0),
                    "{local:?}: {a:?} {b:?}"
                );
            }
            assert_eq!(back.sample_page(2, local), [0.0; 3], "past the last page");
            assert_eq!(back.shade_entry(1, local, [0.0, 1.0, 0.0], false), b);
        }
    }

    /// A one-page lightmap's sections are the bytes version 6 wrote: the version 6 fixture (the
    /// version 5 fixture as the version 6 migrator writes it: the header gains an empty source-hash
    /// list, the sections stay) reads as one page and migrates with the same sections and hash.
    #[test]
    fn version_6_files_read_as_one_page_and_migrate_unchanged() {
        let v6: &[u8] = include_bytes!("../tests/fixtures/lightmap-v6.flmap");
        assert_eq!(lightmap_version(v6).expect("version"), 6);
        let header = Lightmap::read_header(v6).expect("v6 header");
        assert_eq!(header.pages, 1);
        assert!(header.entries.iter().all(|e| e.page == 0));
        let map = Lightmap::from_bytes(v6).expect("v6 reads");
        assert_eq!(map.pages(), 1);
        assert_eq!(
            map.texels,
            fixture().texels,
            "the version 6 texels are the fixture's"
        );
        let v7 = migrate_lightmap_file(v6).expect("migrates");
        assert_eq!(lightmap_version(&v7).expect("version"), LIGHTMAP_FORMAT_VERSION);
        assert_eq!(Lightmap::from_bytes(&v7).expect("v7 reads"), map);
        let h7 = Lightmap::read_header(&v7).expect("v7 header");
        assert_eq!(h7.payload_hash, header.payload_hash, "the payload is untouched");
        for k in 0..LIGHTMAP_SECTIONS {
            let (a, b) = (header.sections[k], h7.sections[k]);
            assert_eq!(
                v6[a.offset as usize..(a.offset + a.len) as usize],
                v7[b.offset as usize..(b.offset + b.len) as usize],
                "section {k} is copied unchanged"
            );
        }
        // Every older version reads as one page too.
        for old in [
            &include_bytes!("../tests/fixtures/lightmap-v1.flmap")[..],
            &include_bytes!("../tests/fixtures/lightmap-v5.flmap")[..],
        ] {
            let m = Lightmap::from_bytes(old).expect("reads");
            assert_eq!(m.pages(), 1);
            assert_eq!(Lightmap::read_header(old).expect("header").pages, 1);
        }
        assert!(lightmap_migration_sources().contains(&6));
    }

    /// Overlap is checked within a page only; texels that do not cover every page, a header whose
    /// page count disagrees with its entries and more than [`MAX_LIGHTMAP_PAGES`] pages are refused.
    #[test]
    fn page_counts_and_overlaps_are_checked_per_page() {
        let map = two_pages();
        map.validate().expect("the same rectangle on two pages is fine");
        let mut same = map.clone();
        same.entries[1].page = 0;
        same.texels.truncate(same.page_texels());
        same.coverage.truncate(same.page_texels());
        assert!(
            format!("{}", same.validate().expect_err("overlap")).contains("overlaps"),
            "one page: the rectangles overlap"
        );
        let mut short = map.clone();
        short.texels.truncate(short.page_texels());
        assert_eq!(
            short.validate().expect_err("one page of texels").code(),
            "lightmap_layout"
        );
        let mut many = map.clone();
        many.entries[1].page = MAX_LIGHTMAP_PAGES;
        assert_eq!(
            many.validate().expect_err("too many pages").code(),
            "lightmap_layout"
        );

        // A header that says 3 pages for entries on 2 (same header length: one varint byte).
        let bytes = map.to_bytes().expect("writes");
        let mut header = Lightmap::read_header(&bytes).expect("header");
        let len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
        header.pages = 3;
        let encoded = postcard::to_allocvec(&header).expect("encodes");
        assert_eq!(encoded.len(), len);
        let mut bad = bytes.clone();
        bad[12..12 + len].copy_from_slice(&encoded);
        assert_eq!(
            Lightmap::from_bytes(&bad).expect_err("3 pages").code(),
            "lightmap_layout"
        );
    }

    /// A block encoding encodes every page on its own: page 0's blocks are the blocks of page 0 alone,
    /// the section is one page's bytes per page, and the file reads back as written. Directions
    /// cover every page, and blocks for one page of a two-page lightmap are refused.
    #[test]
    fn block_encodings_and_directions_work_per_page() {
        for encoding in [LightmapTexelEncoding::Bc6h, LightmapTexelEncoding::AstcHdr] {
            let mut map = two_pages();
            let report = map.encode_texels(encoding).expect("encodes");
            let per_page = encoding.section_bytes(map.width, map.height);
            let blocks = map.blocks.clone().expect("blocks");
            assert_eq!(blocks.data.len(), 2 * per_page);
            assert_eq!(report.bytes, 2 * per_page);
            // Page 0 on its own: the same texel words (the same exposure), one page.
            let mut alone = two_pages();
            alone.entries.truncate(1);
            alone.unwraps.truncate(1);
            alone.texels.truncate(alone.page_texels());
            alone.coverage.truncate(alone.page_texels());
            alone.validate().expect("page 0 alone");
            alone.encode_texels(encoding).expect("encodes");
            assert_eq!(
                blocks.data[..per_page],
                alone.blocks.expect("blocks").data[..],
                "{encoding:?}: page 0 alone"
            );
            let back = Lightmap::from_bytes(&map.to_bytes().expect("writes")).expect("reads");
            assert_eq!(back, map, "{encoding:?}");
        }
        let mut map = two_pages();
        let dirs: Vec<[f32; 4]> = (0..map.total_texels())
            .map(|k| {
                if k < map.page_texels() {
                    [0.0, 1.0, 0.0, 0.5]
                } else {
                    [1.0, 0.0, 0.0, 0.25]
                }
            })
            .collect();
        map.set_directions(&dirs).expect("one per texel of every page");
        assert!(
            map.set_directions(&dirs[..map.page_texels()]).is_err(),
            "one page of directions"
        );
        let back = Lightmap::from_bytes(&map.to_bytes().expect("writes")).expect("reads");
        let d1 = back
            .sample_direction_page(1, [0.25, 0.5])
            .expect("page 1 directions");
        let d0 = back
            .sample_direction_page(0, [0.25, 0.5])
            .expect("page 0 directions");
        assert!(d1[0] > 0.9 && d0[1] > 0.9, "{d0:?} {d1:?}");
        let one_page = vec![0u8; LightmapDirectionEncoding::Bc7.section_bytes(map.width, map.height)];
        assert_eq!(
            map.set_direction_blocks(LightmapDirectionEncoding::Bc7, one_page)
                .expect_err("one page of blocks")
                .code(),
            "lightmap_layout"
        );
    }

    /// The stacked layout puts page `p` below page `p - 1` and keeps a one-page layout as it is; a
    /// packed `scale_offset` is the plain one on page 0, bit for bit, and unpacks to the page.
    #[test]
    fn stacked_layouts_and_packed_scale_offsets_follow_the_page() {
        let map = two_pages();
        let layout = LightmapLayout {
            width: map.width,
            height: map.height,
            density: map.density,
            padding: map.padding,
            entries: map.entries.clone(),
            unwraps: map.unwraps.clone(),
        };
        assert_eq!(layout.pages(), 2);
        let tall = layout.stacked();
        assert_eq!(tall.height, 2 * map.height);
        assert_eq!(tall.entries[1].rect, [0, map.height, 4, 4]);
        assert_eq!(
            tall.entries[1].scale_offset,
            [0.5, 0.5, 0.0, 0.5],
            "into the tall atlas"
        );
        assert_eq!(tall.pages(), 1);
        assert_eq!(tall.stacked(), tall, "one page stays");
        let e = &map.entries[1];
        assert_eq!(map.entries[0].packed_scale_offset(), map.entries[0].scale_offset);
        let packed = e.packed_scale_offset();
        assert_eq!(packed[2], e.scale_offset[2] + LIGHTMAP_PAGE_U_STRIDE);
        assert_eq!(unpack_scale_offset(packed), (1, e.scale_offset));
        let mut far = e.clone();
        far.page = MAX_LIGHTMAP_PAGES - 1;
        far.scale_offset[2] = 0.999_877_9;
        let (page, so) = unpack_scale_offset(far.packed_scale_offset());
        assert_eq!(page, MAX_LIGHTMAP_PAGES - 1);
        assert!((so[2] - far.scale_offset[2]).abs() < 1e-5, "{so:?}");
    }

    #[test]
    fn audfix_from_irradiance_overflow_is_a_layout_error() {
        // `u32::MAX × u32::MAX` fits in a 64-bit usize; a second page does not.
        let entry = LightmapEntry {
            name: "big".into(),
            source_vertices: 0,
            rect: [0, 0, 1, 1],
            scale_offset: [1.0, 1.0, 0.0, 0.0],
            vertex_count: 0,
            index_count: 0,
            first_vertex: 0,
            first_index: 0,
            charts: 0,
            area_m2: 0.0,
            texels_per_metre: 1.0,
            key: None,
            seams: None,
            page: 1,
        };
        let layout = LightmapLayout {
            width: u32::MAX,
            height: u32::MAX,
            density: 1.0,
            padding: 0,
            entries: vec![entry],
            unwraps: vec![MeshUnwrap::default()],
        };
        let err = Lightmap::from_irradiance(layout, &[], Vec::new(), LightmapBakeInfo::default())
            .expect_err("an atlas whose texel count overflows is a layout error");
        assert!(matches!(err, LightmapError::Layout(_)), "{err}");
    }

    #[test]
    fn audfix_total_texels_overflow_does_not_panic() {
        let mut map = fixture();
        map.width = u32::MAX;
        map.height = u32::MAX;
        map.entries[0].page = 1;
        assert_eq!(map.pages(), 2);
        assert_eq!(map.total_texels(), usize::MAX);
    }

    #[test]
    fn audfix_direction_block_page_product_overflow_is_an_error() {
        let mut map = fixture();
        let dirs = vec![[0.0f32; 4]; map.total_texels()];
        map.set_directions(&dirs).expect("directions");
        map.width = 1 << 20;
        map.height = 1 << 20;
        map.entries[0].page = u32::MAX - 1;
        assert_eq!(map.pages(), u32::MAX);
        let err = map
            .set_direction_blocks(LightmapDirectionEncoding::Bc7, Vec::new())
            .expect_err("overflowing direction blocks are a layout error");
        assert!(matches!(err, LightmapError::Layout(_)), "{err}");
    }
}
