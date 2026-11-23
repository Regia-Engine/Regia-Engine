// SPDX-License-Identifier: MPL-2.0
//! Aseprite import (`.aseprite`/`.ase`, `docs/specs/22-tier2.md` § T2.14, ADR-3358): a binary
//! parser for the documented Aseprite file format
//! (<https://github.com/aseprite/aseprite/blob/main/docs/ase-file-specs.md>, cited in
//! `docs/provenance.md`), frame compositing, tag expansion into clips, and atlas-strip building
//! for the existing sprite-atlas cook. Nothing here is derived from any engine's importer; the
//! format is re-implemented from the public specification.
//!
//! Supported: RGBA, grayscale and indexed pixel formats; raw, zlib-compressed and linked cels;
//! normal-blend image layers (opacity multiplied down the group stack, hidden and reference
//! layers skipped, every other blend mode skipped with a warning); the new (0x2019) and old
//! (0x0004/0x0011) palette chunks; frame tags (forward, reverse, ping-pong, ping-pong reverse,
//! repeat counts) with per-frame durations; 9-patch/pivot slices. Tilemap layers/cels (0x2023
//! tilesets) are skipped with a warning. The output feeds `crate::atlas_cook::pack_entries`
//! (plain backticks: the function is private to the crate): one horizontal strip per clip,
//! packed into the cooked [`regia_2d::SpriteAtlas`] — no new
//! runtime format. Clips, layers and warnings are written to the cook's `import_reports.json`
//! (`crate::import_report`) for the agent method `import.report`.

use std::io::Read as _;

use regia_2d::atlas::{NineSlice, RgbaImage};

/// Most warnings one import keeps; the rest are counted in one closing warning.
const MAX_WARNINGS: usize = 256;

/// Hard caps on untrusted input (book/agents/import-report.md lists them).
pub mod limits {
    /// Largest file read (bytes).
    pub const MAX_FILE_BYTES: u64 = 256 << 20;
    /// Largest canvas edge (pixels).
    pub const MAX_DIMENSION: u32 = 16_384;
    /// Most frames.
    pub const MAX_FRAMES: u32 = 4_096;
    /// Most layers.
    pub const MAX_LAYERS: usize = 1_024;
    /// Most frame tags.
    pub const MAX_TAGS: usize = 1_024;
    /// Most slices.
    pub const MAX_SLICES: usize = 1_024;
    /// Most palette entries.
    pub const MAX_PALETTE_ENTRIES: u32 = 1 << 16;
    /// Most decompressed pixels of one cel.
    pub const MAX_CEL_PIXELS: u64 = 64 << 20;
    /// Most pixels in total, counted separately for the decompressed cels, the composited
    /// frames (canvas × frame count) and the clip strips.
    pub const MAX_TOTAL_PIXELS: u64 = 512 << 20;
    /// Most pixel blends while compositing: every drawn cel's on-canvas area summed over all
    /// frames, linked cels included (a linked cel reuses pixels it never paid for at parse time).
    pub const MAX_COMPOSITE_PIXELS: u64 = 4 << 30;
    /// Longest name/string in the file (bytes).
    pub const MAX_STRING: usize = 1 << 16;
}

/// Aseprite import errors; [`AsepriteError::code`] is the stable, documented code
/// (`book/agents/import-report.md`). Every message starts with its code, so a cook log greps it.
#[derive(Debug, thiserror::Error)]
pub enum AsepriteError {
    /// The file ends before a structure completes.
    #[error("aseprite_truncated: {0}")]
    Truncated(&'static str),
    /// The header or a frame header has a wrong magic number.
    #[error("aseprite_bad_magic: {0}")]
    BadMagic(&'static str),
    /// The color depth is not 8, 16 or 32 bits per pixel.
    #[error("aseprite_unsupported_depth: {0} bpp")]
    UnsupportedDepth(u16),
    /// A hard input limit was exceeded.
    #[error("aseprite_limits: {0}")]
    Limits(&'static str),
    /// A zlib stream did not decode to the expected pixels.
    #[error("aseprite_zlib: {0}")]
    Zlib(String),
    /// The file's structures contradict each other.
    #[error("aseprite_structure: {0}")]
    Structure(String),
}

impl AsepriteError {
    /// The stable lowercase code of this error (documented in the book page).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Truncated(_) => "aseprite_truncated",
            Self::BadMagic(_) => "aseprite_bad_magic",
            Self::UnsupportedDepth(_) => "aseprite_unsupported_depth",
            Self::Limits(_) => "aseprite_limits",
            Self::Zlib(_) => "aseprite_zlib",
            Self::Structure(_) => "aseprite_structure",
        }
    }
}

/// Per-source atlas settings from the `.meta` `settings` string (RON, every field optional):
/// `(max_size: 2048, padding: 1, extrude: 0, trim: false)`, the same knobs as `.atlas.ron`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AsepriteImportSettings {
    /// Largest page edge (≥ 16).
    pub max_size: u32,
    /// Transparent pixels between packed sprites.
    pub padding: u32,
    /// Edge pixels repeated outwards.
    pub extrude: u32,
    /// Trim fully transparent borders.
    pub trim: bool,
}

impl Default for AsepriteImportSettings {
    fn default() -> Self {
        Self {
            max_size: 2048,
            padding: 1,
            extrude: 0,
            trim: false,
        }
    }
}

impl AsepriteImportSettings {
    /// Parses the `.meta` `settings` string; `None` (or empty) is the defaults. The values are
    /// checked with [`AsepriteImportSettings::validate`].
    pub fn try_from_meta_string(settings: Option<&str>) -> Result<Self, AsepriteError> {
        let parsed: Self = match settings.map(str::trim) {
            None | Some("") => Self::default(),
            Some(text) => ron::from_str(text)
                .map_err(|e| AsepriteError::Structure(format!("`.meta` settings are not valid RON: {e}")))?,
        };
        parsed.validate()?;
        Ok(parsed)
    }

    /// The same bounds as `.atlas.ron` (`regia_2d::AtlasSource::validate`): `max_size` in
    /// `16..=16384`, `padding` and `extrude` at most 16. Unchecked values overflow the packer's
    /// `width + 2 * extrude` arithmetic or make its extrusion loop run for hours.
    pub fn validate(&self) -> Result<(), AsepriteError> {
        if !(16..=regia_2d::atlas::MAX_PAGE_SIZE).contains(&self.max_size) {
            return Err(AsepriteError::Structure(format!(
                "`.meta` settings: max_size {} is outside 16..={}",
                self.max_size,
                regia_2d::atlas::MAX_PAGE_SIZE
            )));
        }
        if self.padding > MAX_SETTINGS_PADDING || self.extrude > MAX_SETTINGS_PADDING {
            return Err(AsepriteError::Structure(format!(
                "`.meta` settings: padding and extrude may not exceed {MAX_SETTINGS_PADDING}"
            )));
        }
        Ok(())
    }

    /// Digest of the settings for the cache key.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!(
            "max_size={};padding={};extrude={};trim={}",
            self.max_size, self.padding, self.extrude, self.trim
        )
    }
}

/// Largest `padding`/`extrude` in the `.meta` settings (the `.atlas.ron` bound).
const MAX_SETTINGS_PADDING: u32 = 16;

/// One composited frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsepriteFrame {
    /// Frame duration in milliseconds (the frame header field, falling back to the deprecated
    /// header speed, then 100 ms).
    pub duration_ms: u32,
    /// The whole canvas composited (RGBA8, top row first).
    pub image: RgbaImage,
}

/// One layer, as read (for the report).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsepriteLayerInfo {
    /// Layer name.
    pub name: String,
    /// Visible flag (self only; a hidden parent makes it effective-hidden too).
    pub visible: bool,
    /// The layer is a tilemap layer (skipped).
    pub tilemap: bool,
    /// The layer is a reference layer (skipped).
    pub reference: bool,
    /// The layer (or a group above it) uses a blend mode other than Normal and was not composited.
    pub blend_skipped: bool,
}

/// One frame tag, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsepriteTag {
    /// Tag name.
    pub name: String,
    /// First frame (0-based, inclusive).
    pub from: u32,
    /// Last frame (0-based, inclusive).
    pub to: u32,
    /// 0 = forward, 1 = reverse, 2 = ping-pong, 3 = ping-pong reverse.
    pub direction: u8,
    /// `Some(n)` plays the section n times; `None` is unspecified/infinite.
    pub repeat: Option<u32>,
}

/// A tag expanded into playback order: the strip layout and the clip timing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsepriteClip {
    /// Clip name (the tag name, or `default` for a file without tags).
    pub name: String,
    /// Source frames in playback order (a frame may repeat).
    pub frames: Vec<u32>,
    /// Per-entry duration in milliseconds (parallel to `frames`).
    pub durations_ms: Vec<u32>,
    /// `Some(n)` plays the expanded sequence n times; `None` loops forever.
    pub repeat: Option<u32>,
}

/// One atlas entry: a horizontal strip of the clip's composited frames.
#[derive(Clone, Debug, PartialEq)]
pub struct AsepriteSprite {
    /// Sprite name (the clip name).
    pub name: String,
    /// `canvas width × frame count` by `canvas height`, frames left to right.
    pub image: RgbaImage,
    /// Pivot in the frame rectangle: `(0, 0)` bottom-left, `(1, 1)` top-right.
    pub pivot: [f32; 2],
    /// 9-slice borders in frame pixels (from a 9-patch slice), if any.
    pub nine_slice: Option<NineSlice>,
}

/// The imported sprite: composited frames, tags, clips and atlas strips.
#[derive(Clone, Debug)]
pub struct ImportedAseprite {
    /// Canvas size in pixels.
    pub canvas: [u32; 2],
    /// Every composited frame.
    pub frames: Vec<AsepriteFrame>,
    /// Layers in file order (back to front).
    pub layers: Vec<AsepriteLayerInfo>,
    /// Frame tags as read.
    pub tags: Vec<AsepriteTag>,
    /// Clips in tag order (one `default` clip when the file has no tags).
    pub clips: Vec<AsepriteClip>,
    /// Atlas strip entries, one per clip.
    pub sprites: Vec<AsepriteSprite>,
    /// Number of palette entries (0 when the file carries no palette).
    pub palette_size: u32,
    /// Skipped features and recoverable oddities, one line each.
    pub warnings: Vec<String>,
}

const HEADER_MAGIC: u16 = 0xA5E0;
const FRAME_MAGIC: u16 = 0xF1FA;
const FLAG_LAYER_OPACITY_VALID: u32 = 1;
const FLAG_GROUP_BLENDING_VALID: u32 = 2;

/// A bounds-checked little-endian cursor over the file bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], AsepriteError> {
        if self.remaining() < n {
            return Err(AsepriteError::Truncated(what));
        }
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, AsepriteError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, AsepriteError> {
        Ok(u16::from_le_bytes(
            self.take(2, what)?
                .try_into()
                .map_err(|_| AsepriteError::Truncated(what))?,
        ))
    }

    fn i16(&mut self, what: &'static str) -> Result<i16, AsepriteError> {
        Ok(i16::from_le_bytes(
            self.take(2, what)?
                .try_into()
                .map_err(|_| AsepriteError::Truncated(what))?,
        ))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, AsepriteError> {
        Ok(u32::from_le_bytes(
            self.take(4, what)?
                .try_into()
                .map_err(|_| AsepriteError::Truncated(what))?,
        ))
    }

    fn i32(&mut self, what: &'static str) -> Result<i32, AsepriteError> {
        Ok(i32::from_le_bytes(
            self.take(4, what)?
                .try_into()
                .map_err(|_| AsepriteError::Truncated(what))?,
        ))
    }

    fn string(&mut self, what: &'static str) -> Result<String, AsepriteError> {
        let len = usize::from(self.u16(what)?);
        if len > limits::MAX_STRING {
            return Err(AsepriteError::Limits("a string is longer than 64 KiB"));
        }
        let bytes = self.take(len, what)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| AsepriteError::Structure(format!("{what} is not UTF-8")))
    }
}

/// A layer chunk, kept raw until the palette is known.
#[derive(Clone, Debug)]
struct RawLayer {
    flags: u16,
    kind: u16,
    child_level: u16,
    blend: u16,
    opacity: u8,
    name: String,
}

impl RawLayer {
    fn visible(&self) -> bool {
        self.flags & 1 != 0
    }
    fn reference(&self) -> bool {
        self.flags & 64 != 0
    }

    /// Layer flag bit 8: the indexed transparent color does not clear this layer.
    fn background(&self) -> bool {
        self.flags & 8 != 0
    }
}

/// A cel's pixel payload (still in the file's pixel format).
#[derive(Clone, Debug)]
enum CelData {
    /// Width, height and raw pixels.
    Image { w: u16, h: u16, pixels: Vec<u8> },
    /// The cel links to this frame of the same layer.
    Linked { frame: u16 },
}

/// A cel chunk.
#[derive(Clone, Debug)]
struct RawCel {
    layer: u16,
    x: i16,
    y: i16,
    opacity: u8,
    z_index: i16,
    data: CelData,
}

/// A slice key: bounds valid from `frame` until the next key.
#[derive(Clone, Debug)]
struct SliceKey {
    frame: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    /// 9-patch centre relative to the slice bounds.
    center: Option<[i32; 4]>,
    /// Pivot relative to the slice origin.
    pivot: Option<[i32; 2]>,
}

/// A slice: named bounds with per-frame keys.
#[derive(Clone, Debug)]
struct RawSlice {
    name: String,
    keys: Vec<SliceKey>,
}

/// Decodes `compressed` as zlib into exactly `expected` bytes.
fn zlib_decode(compressed: &[u8], expected: usize) -> Result<Vec<u8>, AsepriteError> {
    let decoder = flate2::read::ZlibDecoder::new(compressed);
    let mut out = Vec::with_capacity(expected.min(1 << 20));
    decoder
        .take(expected as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| AsepriteError::Zlib(e.to_string()))?;
    if out.len() != expected {
        return Err(AsepriteError::Zlib(format!(
            "decompressed {} bytes, expected {expected}",
            out.len()
        )));
    }
    Ok(out)
}

/// The effective parent chain of each layer (index into `layers`), built from the child levels
/// (NOTE.1 of the spec): the parent of a layer is the nearest previous layer with a lower child
/// level.
fn parent_indices(layers: &[RawLayer]) -> Vec<Option<usize>> {
    let mut parents = vec![None; layers.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, layer) in layers.iter().enumerate() {
        while let Some(&top) = stack.last()
            && layers[top].child_level >= layer.child_level
        {
            stack.pop();
        }
        parents[i] = stack.last().copied();
        stack.push(i);
    }
    parents
}

/// Integer source-over of one pixel; every operation truncates, so the same input always gives
/// the same output on every platform.
fn blend_pixel(dst: &mut [u8; 4], sr: u8, sg: u8, sb: u8, sa: u8) {
    let sa = u32::from(sa);
    let da = u32::from(dst[3]);
    let oa = sa + da * (255 - sa) / 255;
    if oa == 0 {
        *dst = [0, 0, 0, 0];
        return;
    }
    let channel = |sc: u8, dc: u8| {
        let s = u32::from(sc) * sa;
        let d = u32::from(dc) * da * (255 - sa) / 255;
        ((s + d) / oa) as u8
    };
    dst[0] = channel(sr, dst[0]);
    dst[1] = channel(sg, dst[1]);
    dst[2] = channel(sb, dst[2]);
    dst[3] = oa as u8;
}

/// Expands a tag's `[from, to]` range into playback order (`direction` per the spec:
/// 0 forward, 1 reverse, 2 ping-pong, 3 ping-pong reverse).
fn expand_range(from: u32, to: u32, direction: u8) -> Vec<u32> {
    let base: Vec<u32> = (from..=to).collect();
    let interior_reversed = |v: &[u32]| -> Vec<u32> {
        if v.len() > 2 {
            v[1..v.len() - 1].iter().rev().copied().collect()
        } else {
            Vec::new()
        }
    };
    match direction {
        1 => base.iter().rev().copied().collect(),
        2 => {
            let mut out = base.clone();
            out.extend(interior_reversed(&base));
            out
        }
        3 => {
            let reversed: Vec<u32> = base.iter().rev().copied().collect();
            let mut out = reversed.clone();
            out.extend(interior_reversed(&reversed));
            out
        }
        _ => base,
    }
}

/// Imports an Aseprite file: parses, composites, expands tags and builds the atlas strips.
///
/// `stem` names the `default` clip of a file without tags. Every size, count and decompressed
/// byte is bounded by [`limits`]; failures carry a documented code ([`AsepriteError::code`]).
pub fn import_aseprite(bytes: &[u8], stem: &str) -> Result<ImportedAseprite, AsepriteError> {
    if bytes.len() as u64 > limits::MAX_FILE_BYTES {
        return Err(AsepriteError::Limits("file is larger than 256 MiB"));
    }
    let mut reader = Reader::new(bytes);
    // -- Header --------------------------------------------------------------
    let header_size = reader.u32("header")?;
    let magic = reader.u16("header")?;
    if magic != HEADER_MAGIC {
        return Err(AsepriteError::BadMagic("file header"));
    }
    let frame_count = u32::from(reader.u16("header")?);
    let width = u32::from(reader.u16("header")?);
    let height = u32::from(reader.u16("header")?);
    let depth = reader.u16("header")?;
    let flags = reader.u32("header")?;
    let speed = reader.u16("header")?;
    reader.take(4, "header")?; // Deprecated, must be 0.
    reader.take(4, "header")?;
    let transparent_index = reader.u8("header")?;
    reader.take(3, "header")?;
    let _color_count = reader.u16("header")?;
    reader.take(2, "header")?; // Pixel ratio.
    reader.take(8, "header")?; // Grid position and size.
    reader.take(84, "header")?;

    if header_size as usize > bytes.len() || header_size < 128 {
        return Err(AsepriteError::Truncated("file is shorter than its header says"));
    }
    if width == 0 || height == 0 || width > limits::MAX_DIMENSION || height > limits::MAX_DIMENSION {
        return Err(AsepriteError::Limits("canvas size is 0 or above 16384 px"));
    }
    if frame_count == 0 || frame_count > limits::MAX_FRAMES {
        return Err(AsepriteError::Limits("frame count is 0 or above 4096"));
    }
    let bytes_per_pixel: u16 = match depth {
        32 => 4,
        16 => 2,
        8 => 1,
        other => return Err(AsepriteError::UnsupportedDepth(other)),
    };
    let canvas_pixels = u64::from(width) * u64::from(height);
    // Every frame is composited into its own canvas and kept, so the frames cost canvas × count
    // pixels whatever the cels hold: bound it before any canvas is allocated.
    if canvas_pixels * u64::from(frame_count) > limits::MAX_TOTAL_PIXELS {
        return Err(AsepriteError::Limits(
            "the composited frames (canvas × frame count) exceed 512 Mpx",
        ));
    }

    // -- Frames and chunks ---------------------------------------------------
    let mut layers: Vec<RawLayer> = Vec::new();
    let mut cels: Vec<Vec<RawCel>> = vec![Vec::new(); frame_count as usize];
    let mut durations: Vec<u32> = vec![0; frame_count as usize];
    let mut palette: Option<Vec<[u8; 4]>> = None;
    let mut old_palette: Option<Vec<[u8; 4]>> = None;
    let mut tags: Vec<AsepriteTag> = Vec::new();
    let mut slices: Vec<RawSlice> = Vec::new();
    let mut warnings = CappedWarnings::default();
    let mut total_cel_pixels: u64 = 0;

    for frame_index in 0..frame_count {
        let frame_start = reader.pos;
        let frame_size = reader.u32("frame header")? as usize;
        if reader.u16("frame header")? != FRAME_MAGIC {
            return Err(AsepriteError::BadMagic("frame header"));
        }
        let old_chunk_count = reader.u16("frame header")?;
        let duration = reader.u16("frame header")?;
        reader.take(2, "frame header")?;
        let new_chunk_count = reader.u32("frame header")?;
        let chunk_count = if new_chunk_count == 0 {
            u32::from(old_chunk_count)
        } else {
            new_chunk_count
        };
        durations[frame_index as usize] = if duration == 0 {
            u32::from(speed)
        } else {
            u32::from(duration)
        };
        if frame_size < 16 || frame_start + frame_size > bytes.len() {
            return Err(AsepriteError::Truncated("frame overruns the file"));
        }
        let frame_end = frame_start + frame_size;

        for _ in 0..chunk_count {
            let chunk_start = reader.pos;
            if chunk_start + 6 > frame_end {
                return Err(AsepriteError::Truncated("chunk header overruns the frame"));
            }
            let chunk_size = reader.u32("chunk")? as usize;
            let chunk_type = reader.u16("chunk")?;
            if chunk_size < 6 || chunk_start + chunk_size > frame_end {
                return Err(AsepriteError::Structure(format!(
                    "chunk 0x{chunk_type:04x} overruns its frame"
                )));
            }
            let chunk_end = chunk_start + chunk_size;
            let mut chunk = Reader::new(&bytes[reader.pos..chunk_end]);
            match chunk_type {
                0x2004 => {
                    // Layer.
                    if layers.len() >= limits::MAX_LAYERS {
                        return Err(AsepriteError::Limits("more than 1024 layers"));
                    }
                    let flags = chunk.u16("layer")?;
                    let kind = chunk.u16("layer")?;
                    let child_level = chunk.u16("layer")?;
                    chunk.take(4, "layer")?; // Ignored default size.
                    let blend = chunk.u16("layer")?;
                    let opacity = chunk.u8("layer")?;
                    chunk.take(3, "layer")?;
                    let name = chunk.string("layer")?;
                    if kind == 2 {
                        let _tileset_index = chunk.u32("layer")?;
                    }
                    layers.push(RawLayer {
                        flags,
                        kind,
                        child_level,
                        blend,
                        opacity,
                        name,
                    });
                }
                0x2005 => {
                    // Cel.
                    let layer = chunk.u16("cel")?;
                    let x = chunk.i16("cel")?;
                    let y = chunk.i16("cel")?;
                    let opacity = chunk.u8("cel")?;
                    let cel_type = chunk.u16("cel")?;
                    let z_index = chunk.i16("cel")?;
                    chunk.take(5, "cel")?;
                    let data = match cel_type {
                        0 | 2 => {
                            let w = chunk.u16("cel")?;
                            let h = chunk.u16("cel")?;
                            let pixels = u64::from(w) * u64::from(h);
                            if pixels == 0 || pixels > limits::MAX_CEL_PIXELS {
                                return Err(AsepriteError::Limits(
                                    "a cel decompressed above 64 Mpx or is empty",
                                ));
                            }
                            total_cel_pixels += pixels;
                            if total_cel_pixels > limits::MAX_TOTAL_PIXELS {
                                return Err(AsepriteError::Limits(
                                    "the cels decompress above 512 Mpx in total",
                                ));
                            }
                            let expected = pixels * u64::from(bytes_per_pixel);
                            let rest = chunk.take(chunk.remaining(), "cel")?;
                            let pixels = if cel_type == 0 {
                                if rest.len() as u64 != expected {
                                    return Err(AsepriteError::Structure(format!(
                                        "raw cel is {} bytes, expected {expected}",
                                        rest.len()
                                    )));
                                }
                                rest.to_vec()
                            } else {
                                zlib_decode(rest, expected as usize)?
                            };
                            CelData::Image { w, h, pixels }
                        }
                        1 => CelData::Linked {
                            frame: chunk.u16("cel")?,
                        },
                        3 => {
                            // Compressed tilemap: the layer is skipped anyway; skip the payload.
                            chunk.take(chunk.remaining(), "cel")?;
                            warnings.push(format!(
                                "frame {frame_index}: a tilemap cel was skipped (tilemap layers are not imported)"
                            ));
                            // Replace below with a marker: store as a zero-sized image cel is
                            // wrong; use a linked-to-self sentinel is worse. Skip via Option.
                            // Handled by leaving `data` set to a linked-to-own-frame marker.
                            CelData::Linked {
                                frame: frame_index as u16,
                            }
                        }
                        other => {
                            return Err(AsepriteError::Structure(format!("unknown cel type {other}")));
                        }
                    };
                    cels[frame_index as usize].push(RawCel {
                        layer,
                        x,
                        y,
                        opacity,
                        z_index,
                        data,
                    });
                }
                0x2019 => {
                    // New palette.
                    let size = chunk.u32("palette")?;
                    let first = chunk.u32("palette")?;
                    let last = chunk.u32("palette")?;
                    chunk.take(8, "palette")?;
                    if size == 0 || size > limits::MAX_PALETTE_ENTRIES || first > last || last >= size {
                        return Err(AsepriteError::Limits(
                            "palette bounds are inconsistent or above 65536 entries",
                        ));
                    }
                    let mut entries = vec![[0u8; 4]; size as usize];
                    for index in first..=last {
                        let entry_flags = chunk.u16("palette")?;
                        let r = chunk.u8("palette")?;
                        let g = chunk.u8("palette")?;
                        let b = chunk.u8("palette")?;
                        let a = chunk.u8("palette")?;
                        entries[index as usize] = [r, g, b, a];
                        if entry_flags & 1 != 0 {
                            let _name = chunk.string("palette")?;
                        }
                    }
                    palette = Some(entries);
                }
                0x0004 | 0x0011 => {
                    // Old palettes. The file spec says to ignore this chunk once a new palette
                    // (0x2019) has been read: Aseprite 1.1 writes both, and a truncated old chunk
                    // must not fail a file that already has the new one. A packet's count byte is
                    // the number of colors, and 0 means 256 (the field cannot store 256). 0x0011
                    // stores 6-bit channels.
                    if palette.is_none() {
                        // 6-bit to 8-bit by bit replication, so 63 maps to 255.
                        let shift = |v: u8| {
                            if chunk_type == 0x0011 {
                                let v = v.min(63);
                                (v << 2) | (v >> 4)
                            } else {
                                v
                            }
                        };
                        let packets = chunk.u16("palette")?;
                        let mut index: usize = 0;
                        for _ in 0..packets {
                            let skip = usize::from(chunk.u8("palette")?);
                            let raw_count = usize::from(chunk.u8("palette")?);
                            let count = if raw_count == 0 { 256 } else { raw_count };
                            index = index.saturating_add(skip);
                            let entries = old_palette.get_or_insert_with(|| vec![[0u8; 4]; 256]);
                            for _ in 0..count {
                                let r = shift(chunk.u8("palette")?);
                                let g = shift(chunk.u8("palette")?);
                                let b = shift(chunk.u8("palette")?);
                                if index < entries.len() {
                                    entries[index] = [r, g, b, 255];
                                }
                                index = index.saturating_add(1);
                            }
                        }
                    }
                }
                0x2018 => {
                    // Frame tags.
                    let count = usize::from(chunk.u16("tags")?);
                    chunk.take(8, "tags")?;
                    if tags.len() + count > limits::MAX_TAGS {
                        return Err(AsepriteError::Limits("more than 1024 frame tags"));
                    }
                    for _ in 0..count {
                        let from = u32::from(chunk.u16("tags")?);
                        let to = u32::from(chunk.u16("tags")?);
                        let direction = chunk.u8("tags")?;
                        let repeat = u32::from(chunk.u16("tags")?);
                        chunk.take(6, "tags")?;
                        chunk.take(3, "tags")?; // Deprecated tag colour.
                        chunk.take(1, "tags")?;
                        let name = chunk.string("tags")?;
                        if from > to || to >= frame_count {
                            return Err(AsepriteError::Structure(format!(
                                "tag `{name}` covers frames {from}..={to} of {frame_count}"
                            )));
                        }
                        tags.push(AsepriteTag {
                            name,
                            from,
                            to,
                            direction,
                            repeat: (repeat > 0).then_some(repeat),
                        });
                    }
                }
                0x2022 => {
                    // Slice.
                    if slices.len() >= limits::MAX_SLICES {
                        return Err(AsepriteError::Limits("more than 1024 slices"));
                    }
                    let key_count = chunk.u32("slice")?;
                    let slice_flags = chunk.u32("slice")?;
                    chunk.take(4, "slice")?;
                    if key_count == 0 || key_count > frame_count {
                        return Err(AsepriteError::Structure(format!(
                            "slice has {key_count} keys for {frame_count} frames"
                        )));
                    }
                    let name = chunk.string("slice")?;
                    let mut keys = Vec::with_capacity(key_count as usize);
                    for _ in 0..key_count {
                        let frame = chunk.u32("slice")?;
                        let x = chunk.i32("slice")?;
                        let y = chunk.i32("slice")?;
                        let w = chunk.u32("slice")?;
                        let h = chunk.u32("slice")?;
                        if frame >= frame_count {
                            return Err(AsepriteError::Structure(format!(
                                "slice `{name}` has a key at frame {frame} of {frame_count}"
                            )));
                        }
                        let center = if slice_flags & 1 != 0 {
                            Some([
                                chunk.i32("slice")?,
                                chunk.i32("slice")?,
                                chunk.u32("slice")? as i32,
                                chunk.u32("slice")? as i32,
                            ])
                        } else {
                            None
                        };
                        let pivot = if slice_flags & 2 != 0 {
                            Some([chunk.i32("slice")?, chunk.i32("slice")?])
                        } else {
                            None
                        };
                        keys.push(SliceKey {
                            frame,
                            x,
                            y,
                            w,
                            h,
                            center,
                            pivot,
                        });
                    }
                    slices.push(RawSlice { name, keys });
                }
                0x2023 => {
                    // Tileset.
                    warnings
                        .push("a tileset chunk was skipped (tilemap layers are not imported)".to_string());
                }
                _ => {
                    // Unknown, mask, path, color profile, user data, ... : skipped.
                }
            }
            reader.pos = chunk_end;
        }
        if reader.pos != frame_end {
            return Err(AsepriteError::Structure(format!(
                "frame {frame_index} ends at byte {} but the header says {frame_end}",
                reader.pos
            )));
        }
    }

    // -- Layers --------------------------------------------------------------
    if layers.is_empty() {
        return Err(AsepriteError::Structure("the file has no layers".into()));
    }
    let parents = parent_indices(&layers);
    let layer_info: Vec<AsepriteLayerInfo> = layers
        .iter()
        .map(|l| AsepriteLayerInfo {
            name: l.name.clone(),
            visible: l.visible(),
            tilemap: l.kind == 2,
            reference: l.reference(),
            blend_skipped: false,
        })
        .collect();

    // Effective opacity (0..=255) and compositability for each layer: groups multiply their
    // opacity into their children; a non-Normal blend anywhere in the chain skips the layer
    // (with a warning), unless the header says group blending is invalid, in which case the
    // group's own blend/opacity is ignored. A hidden ancestor hides the layer silently.
    let mut effective_opacity: Vec<u16> = vec![255; layers.len()];
    let mut compositable: Vec<bool> = vec![false; layers.len()];
    let mut blend_skipped: Vec<bool> = vec![false; layers.len()];
    for i in 0..layers.len() {
        let mut chain = vec![i];
        let mut parent = parents[i];
        while let Some(p) = parent {
            chain.push(p);
            parent = parents[p];
        }
        let mut opacity: u32 = 255;
        let mut skipped = false;
        for &index in &chain {
            let layer = &layers[index];
            let is_group = layer.kind == 1;
            if !is_group {
                if layer.kind == 2 || layer.reference() {
                    skipped = true;
                    break;
                }
                if layer.blend != 0 {
                    skipped = true;
                    blend_skipped[i] = true;
                    warnings.push(format!(
                        "layer `{}` uses blend mode {} (not Normal) and was not composited",
                        layer.name, layer.blend
                    ));
                    break;
                }
                let own = if flags & FLAG_LAYER_OPACITY_VALID != 0 {
                    u32::from(layer.opacity)
                } else {
                    255
                };
                opacity = opacity * own / 255;
            } else if flags & FLAG_GROUP_BLENDING_VALID != 0 {
                if layer.blend != 0 {
                    skipped = true;
                    blend_skipped[i] = true;
                    warnings.push(format!(
                        "group `{}` uses blend mode {} (not Normal) and was not composited",
                        layer.name, layer.blend
                    ));
                    break;
                }
                opacity = opacity * u32::from(layer.opacity) / 255;
            }
        }
        blend_skipped[i] |= layers[i].kind == 2 || layers[i].reference();
        let hidden_by_parent = chain.iter().skip(1).any(|&index| !layers[index].visible());
        compositable[i] = !skipped && layers[i].visible() && !hidden_by_parent;
        effective_opacity[i] = if compositable[i] {
            opacity.min(255) as u16
        } else {
            0
        };
    }
    let mut layer_info = layer_info;
    for (i, info) in layer_info.iter_mut().enumerate() {
        info.blend_skipped = blend_skipped[i];
    }

    let palette = match (&palette, &old_palette) {
        (Some(new), _) => Some(new.clone()),
        (None, Some(old)) => Some(old.clone()),
        (None, None) => None,
    };
    let palette_size = palette.as_ref().map_or(0, |p| p.len() as u32);
    if depth == 8 && palette.is_none() {
        return Err(AsepriteError::Structure(
            "an indexed sprite carries no palette".into(),
        ));
    }

    // -- Compositing ---------------------------------------------------------
    // Budget the blend work first: linked cels reuse pixels without paying for them at parse
    // time, so a small file of many links to one large cel would otherwise composite for hours.
    let mut composite_pixels: u64 = 0;
    for frame_cels in &cels {
        for cel in frame_cels {
            let layer_index = usize::from(cel.layer);
            if !compositable.get(layer_index).copied().unwrap_or(false) {
                continue;
            }
            if let Ok((w, h, _)) = resolve_cel_pixels(&cels, cel, frame_count) {
                composite_pixels += canvas_overlap(cel.x, cel.y, w, h, width, height)
                    .map_or(0, |[x0, x1, y0, y1]| u64::from(x1 - x0) * u64::from(y1 - y0));
                if composite_pixels > limits::MAX_COMPOSITE_PIXELS {
                    return Err(AsepriteError::Limits(
                        "compositing the cels (linked cels included) exceeds 4 Gpx of blends",
                    ));
                }
            }
        }
    }
    let mut frames: Vec<AsepriteFrame> = Vec::with_capacity(frame_count as usize);
    for (frame_index, frame_cels) in cels.iter().enumerate() {
        let mut canvas = vec![[0u8; 4]; canvas_pixels as usize];
        // Back to front (NOTE.5): by layerIndex + zIndex, ties by zIndex.
        let mut ordered: Vec<&RawCel> = frame_cels.iter().collect();
        ordered.sort_by_key(|cel| {
            let order = i32::from(cel.layer) + i32::from(cel.z_index);
            (order, i32::from(cel.z_index))
        });
        for cel in ordered {
            let layer_index = usize::from(cel.layer);
            if layer_index >= layers.len() {
                warnings.push(format!(
                    "frame {frame_index}: a cel references layer {} of {}",
                    cel.layer,
                    layers.len()
                ));
                continue;
            }
            if !compositable[layer_index] {
                continue;
            }
            // Resolve linked cels within the same layer. The link supplies pixels only:
            // this cel's own header keeps the position and opacity (file spec, cel chunk).
            let (place_x, place_y, place_opacity) = (cel.x, cel.y, cel.opacity);
            let (w, h, pixels) = match resolve_cel_pixels(&cels, cel, frame_count) {
                Ok(found) => found,
                Err(LinkError::Loop) => {
                    warnings.push(format!(
                        "frame {frame_index}: a linked cel chains in a loop on layer {}",
                        cel.layer
                    ));
                    continue;
                }
                Err(LinkError::Missing(source_frame)) => {
                    warnings.push(format!(
                        "frame {frame_index}: a cel links to frame {source_frame} which has no cel on layer {}",
                        cel.layer
                    ));
                    continue;
                }
            };
            if pixels.len() as u64 != u64::from(w) * u64::from(h) * u64::from(bytes_per_pixel) {
                continue; // warned at parse time
            }
            // Only the part of the cel that lands on the canvas is visited.
            let Some([x0, x1, y0, y1]) = canvas_overlap(place_x, place_y, w, h, width, height) else {
                continue;
            };
            let alpha = u32::from(place_opacity) * u32::from(effective_opacity[layer_index]);
            for py in y0..y1 {
                for px in x0..x1 {
                    let (dx, dy) = (i32::from(place_x) + px as i32, i32::from(place_y) + py as i32);
                    let at = (py as usize) * usize::from(w) + px as usize;
                    let (r, g, b, a) = match bytes_per_pixel {
                        4 => {
                            let p = &pixels[at * 4..at * 4 + 4];
                            (p[0], p[1], p[2], p[3])
                        }
                        2 => {
                            let p = &pixels[at * 2..at * 2 + 2];
                            (p[0], p[0], p[0], p[1])
                        }
                        _ => {
                            let index = pixels[at];
                            let [r, g, b, mut a] = palette
                                .as_ref()
                                .and_then(|p| p.get(usize::from(index)).copied())
                                .unwrap_or([0, 0, 0, 255]);
                            // The transparent index clears every non-background layer. A
                            // background layer keeps the palette color (file spec, header).
                            if index == transparent_index && !layers[layer_index].background() {
                                a = 0;
                            }
                            (r, g, b, a)
                        }
                    };
                    let a = (u32::from(a) * alpha / 65_025) as u8;
                    if a == 0 {
                        continue;
                    }
                    let out = &mut canvas[(dy as usize) * (width as usize) + dx as usize];
                    blend_pixel(out, r, g, b, a);
                }
            }
        }
        let mut rgba8 = Vec::with_capacity(canvas_pixels as usize * 4);
        for pixel in &canvas {
            rgba8.extend_from_slice(pixel);
        }
        frames.push(AsepriteFrame {
            duration_ms: if durations[frame_index] == 0 {
                100
            } else {
                durations[frame_index]
            },
            image: RgbaImage { width, height, rgba8 },
        });
    }

    // -- Clips ---------------------------------------------------------------
    let clips_from_tags: Vec<(String, Vec<u32>, Option<u32>)> = if tags.is_empty() {
        vec![("default".to_string(), (0..frame_count).collect(), None)]
    } else {
        tags.iter()
            .map(|tag| {
                (
                    tag.name.clone(),
                    expand_range(tag.from, tag.to, tag.direction),
                    tag.repeat,
                )
            })
            .collect()
    };
    let mut clips: Vec<AsepriteClip> = clips_from_tags
        .into_iter()
        .map(|(name, frames_seq, repeat)| {
            let durations_ms = frames_seq
                .iter()
                .map(|&f| frames[f as usize].duration_ms)
                .collect();
            AsepriteClip {
                name,
                frames: frames_seq,
                durations_ms,
                repeat,
            }
        })
        .collect();

    // -- Slices -> pivot / 9-slice -------------------------------------------
    // A slice attaches to the first clip (in clip order) that contains one of its frames; the
    // key active at the clip's first such frame decides (bounds varying inside a clip warn).
    let mut sprite_meta: Vec<(Option<NineSlice>, Option<[f32; 2]>)> = vec![(None, None); clips.len()];
    let mut slice_used = vec![false; slices.len()];
    for (clip_index, clip) in clips.iter().enumerate() {
        for (slice_index, slice) in slices.iter().enumerate() {
            // In playback order: a reverse clip starts at its highest frame.
            let active = clip.frames.iter().find_map(|&f| {
                slice
                    .keys
                    .iter()
                    .filter(|key| key.frame <= f)
                    .max_by_key(|key| key.frame)
                    .map(|key| (f, key))
            });
            let Some((first_frame, key)) = active else {
                continue;
            };
            let varying = clip.frames.iter().any(|&f| {
                slice
                    .keys
                    .iter()
                    .filter(|k| k.frame <= f)
                    .max_by_key(|k| k.frame)
                    .is_some_and(|k| {
                        k.x != key.x || k.y != key.y || k.w != key.w || k.h != key.h || k.center != key.center
                    })
            });
            if varying {
                warnings.push(format!(
                    "slice `{}` bounds vary inside clip `{}`; the frame {first_frame} key was used",
                    slice.name, clip.name
                ));
            }
            if slice_used[slice_index] {
                warnings.push(format!(
                    "slice `{}` also covers clip `{}` (already attached to another clip); ignored here",
                    slice.name, clip.name
                ));
                continue;
            }
            slice_used[slice_index] = true;
            let (nine_slice, pivot) = sprite_meta[clip_index];
            if key.w == 0 || key.h == 0 {
                warnings.push(format!(
                    "slice `{}` is empty at frame {first_frame}; ignored",
                    slice.name
                ));
                continue;
            }
            // The file stores the 9-patch centre relative to the slice bounds and the pivot
            // relative to the slice origin; the sprite is the whole frame, so both move into
            // frame coordinates (borders in frame pixels, pivot normalized to the frame).
            let (slice_x, slice_y) = (i64::from(key.x), i64::from(key.y));
            let (frame_w, frame_h) = (i64::from(width), i64::from(height));
            let nine_slice = nine_slice.or_else(|| {
                key.center.map(|[cx, cy, cw, ch]| {
                    let left = slice_x + i64::from(cx);
                    let top = slice_y + i64::from(cy);
                    let border = |v: i64, edge: i64| v.clamp(0, edge) as u32;
                    NineSlice::from_array([
                        border(left, frame_w),
                        border(top, frame_h),
                        border(frame_w - left - i64::from(cw), frame_w),
                        border(frame_h - top - i64::from(ch), frame_h),
                    ])
                })
            });
            // Canvas coordinates are +Y down; the atlas pivot is +Y up in the frame rect.
            let pivot = pivot.or_else(|| {
                key.pivot.map(|[px, py]| {
                    [
                        (slice_x + i64::from(px)) as f32 / width as f32,
                        1.0 - (slice_y + i64::from(py)) as f32 / height as f32,
                    ]
                })
            });
            sprite_meta[clip_index] = (nine_slice, pivot);
        }
    }
    for (slice_index, slice) in slices.iter().enumerate() {
        if !slice_used[slice_index] {
            warnings.push(format!(
                "slice `{}` covers no frame of any clip and was not exported",
                slice.name
            ));
        }
    }

    // -- Strips ---------------------------------------------------------------
    let mut sprites: Vec<AsepriteSprite> = Vec::with_capacity(clips.len());
    let mut strip_pixels: u64 = 0;
    for (index, clip) in clips.iter().enumerate() {
        let (nine_slice, pivot) = sprite_meta[index];
        let strip_w = width
            .checked_mul(clip.frames.len() as u32)
            .filter(|w| *w <= limits::MAX_DIMENSION && *w > 0)
            .ok_or(AsepriteError::Limits("a clip strip is wider than 16384 px"))?;
        strip_pixels += u64::from(strip_w) * u64::from(height);
        if strip_pixels > limits::MAX_TOTAL_PIXELS {
            return Err(AsepriteError::Limits("the strips exceed 512 Mpx in total"));
        }
        let mut image = RgbaImage {
            width: strip_w,
            height,
            rgba8: vec![0; strip_w as usize * height as usize * 4],
        };
        for (slot, &frame_index) in clip.frames.iter().enumerate() {
            let frame = &frames[frame_index as usize].image;
            for row in 0..height as usize {
                let src = row * width as usize * 4;
                let dst = (row * strip_w as usize + slot * width as usize) * 4;
                image.rgba8[dst..dst + width as usize * 4]
                    .copy_from_slice(&frame.rgba8[src..src + width as usize * 4]);
            }
        }
        sprites.push(AsepriteSprite {
            name: clip.name.clone(),
            image,
            pivot: pivot.unwrap_or([0.5, 0.5]),
            nine_slice,
        });
    }

    // A file without tags gets its clip named after the file (a sprite named `default` would
    // collide between files in an atlas).
    if tags.is_empty() && !clips.is_empty() {
        let name = format!("{stem}_default");
        clips[0].name = name.clone();
        sprites[0].name = name;
    }

    Ok(ImportedAseprite {
        canvas: [width, height],
        frames,
        layers: layer_info,
        tags,
        clips,
        sprites,
        palette_size,
        warnings: warnings.finish(),
    })
}

/// The warnings of one import, capped at `MAX_WARNINGS`: a file of millions of bad cels must
/// not hold (or write to the import report) a line for each.
#[derive(Default)]
struct CappedWarnings {
    kept: Vec<String>,
    dropped: usize,
}

impl CappedWarnings {
    fn push(&mut self, warning: String) {
        if self.kept.len() < MAX_WARNINGS {
            self.kept.push(warning);
        } else {
            self.dropped += 1;
        }
    }

    /// The kept warnings, closed by one line counting the dropped ones.
    fn finish(mut self) -> Vec<String> {
        if self.dropped > 0 {
            self.kept.push(format!(
                "{} more warnings were not listed (at most {MAX_WARNINGS} are kept)",
                self.dropped
            ));
        }
        self.kept
    }
}

/// Why a linked cel found no pixels.
#[derive(Debug, PartialEq, Eq)]
enum LinkError {
    /// The links chain in a loop (longer than the frame count).
    Loop,
    /// The linked frame has no cel on the layer.
    Missing(u16),
}

/// The pixels a cel draws: its own image, or the image a chain of linked cels on the same layer
/// ends at. The link supplies pixels only; the caller keeps the cel's own position and opacity
/// (file spec, cel chunk).
fn resolve_cel_pixels<'a>(
    cels: &'a [Vec<RawCel>],
    cel: &'a RawCel,
    frame_count: u32,
) -> Result<(u16, u16, &'a [u8]), LinkError> {
    let mut cel = cel;
    let mut guard = 0u32;
    loop {
        match &cel.data {
            CelData::Image { w, h, pixels } => return Ok((*w, *h, pixels)),
            CelData::Linked { frame } => {
                if guard >= frame_count {
                    return Err(LinkError::Loop);
                }
                guard += 1;
                cel = cels
                    .get(usize::from(*frame))
                    .and_then(|fs| fs.iter().find(|c| c.layer == cel.layer))
                    .ok_or(LinkError::Missing(*frame))?;
            }
        }
    }
}

/// The cel-local pixel range `[x0, x1, y0, y1]` (half-open) of a `w × h` cel placed at `(x, y)`
/// that lands on a `width × height` canvas, or `None` when nothing does.
fn canvas_overlap(x: i16, y: i16, w: u16, h: u16, width: u32, height: u32) -> Option<[u32; 4]> {
    let span = |at: i16, len: u16, canvas: u32| -> Option<(u32, u32)> {
        let (at, len, canvas) = (i64::from(at), i64::from(len), i64::from(canvas));
        let start = (-at).clamp(0, len);
        let end = (canvas - at).clamp(0, len);
        (start < end).then_some((start as u32, end as u32))
    };
    let (x0, x1) = span(x, w, width)?;
    let (y0, y1) = span(y, h, height)?;
    Some([x0, x1, y0, y1])
}

#[cfg(test)]
pub(crate) mod tests {
    //! The fixtures are built by hand through [`Writer`]: a small Aseprite *writer* in the tests
    //! (the brief forbids checking in third-party art). Every test writes the bytes it reads.

    use super::*;

    /// Little-endian byte writer for fixture files.
    #[derive(Default)]
    pub(crate) struct Writer {
        pub bytes: Vec<u8>,
    }

    impl Writer {
        pub fn u8(&mut self, v: u8) {
            self.bytes.push(v);
        }
        pub fn u16(&mut self, v: u16) {
            self.bytes.extend(v.to_le_bytes());
        }
        pub fn i16(&mut self, v: i16) {
            self.bytes.extend(v.to_le_bytes());
        }
        pub fn u32(&mut self, v: u32) {
            self.bytes.extend(v.to_le_bytes());
        }
        pub fn i32(&mut self, v: i32) {
            self.bytes.extend(v.to_le_bytes());
        }
        pub fn raw(&mut self, bytes: &[u8]) {
            self.bytes.extend_from_slice(bytes);
        }
        pub fn string(&mut self, s: &str) {
            self.u16(s.len() as u16);
            self.raw(s.as_bytes());
        }
    }

    /// One layer of a fixture.
    pub(crate) struct TestLayer {
        pub name: &'static str,
        pub visible: bool,
        pub opacity: u8,
        pub blend: u16,
    }

    /// One cel of a fixture: pixels in the file's pixel format.
    #[derive(Clone)]
    pub(crate) enum TestCel {
        /// Compressed image cel at a position.
        Image {
            x: i16,
            y: i16,
            w: u16,
            h: u16,
            pixels: Vec<u8>,
        },
        /// Raw (uncompressed) image cel.
        RawImage {
            x: i16,
            y: i16,
            w: u16,
            h: u16,
            pixels: Vec<u8>,
        },
        /// Linked to a frame of the same layer.
        Linked { frame: u16 },
    }

    /// A frame tag for the fixture writer.
    pub(crate) struct TestTag {
        pub name: &'static str,
        pub from: u16,
        pub to: u16,
        pub direction: u8,
        pub repeat: u16,
    }

    /// Builds a complete fixture file. `chunks0` are extra chunk bodies (tags, slices, palette)
    /// appended to frame 0 after the layer and cel chunks; `frame_chunks[i]` are extra chunks for
    /// frame i (already-encoded chunk bodies).
    /// The fixture file to write ([`build_file`]).
    pub(crate) struct TestFile<'a> {
        pub width: u16,
        pub height: u16,
        pub depth: u16,
        pub transparent_index: u8,
        pub speed_ms: u16,
        pub layers: &'a [TestLayer],
        /// Per frame: (layer index, cel).
        pub cels: &'a [Vec<(u16, TestCel)>],
        pub durations: &'a [u16],
        pub frame0_chunks: Vec<(u16, Vec<u8>)>,
        pub other_chunks: &'a [Vec<(u16, Vec<u8>)>],
        pub palette: Option<&'a [[u8; 4]]>,
    }

    /// Builds a complete fixture file from `spec`.
    pub(crate) fn build_file(spec: &TestFile) -> Vec<u8> {
        let width = spec.width;
        let height = spec.height;
        let depth = spec.depth;
        let transparent_index = spec.transparent_index;
        let speed_ms = spec.speed_ms;
        let layers = spec.layers;
        let cels = spec.cels;
        let durations = spec.durations;
        let frame0_chunks = &spec.frame0_chunks;
        let other_chunks = spec.other_chunks;
        let palette = spec.palette;
        let mut frame_bodies: Vec<(u32, Vec<u8>)> = Vec::new();
        for (frame_index, frame_cels) in cels.iter().enumerate() {
            let mut body = Writer::default();
            let mut count = 0u32;
            if frame_index == 0 {
                for layer in layers {
                    let mut chunk = Writer::default();
                    chunk.u16(if layer.visible { 1 } else { 0 });
                    chunk.u16(0); // Normal layer.
                    chunk.u16(0); // Child level.
                    chunk.u16(0);
                    chunk.u16(0);
                    chunk.u16(layer.blend);
                    chunk.u8(layer.opacity);
                    chunk.raw(&[0; 3]);
                    chunk.string(layer.name);
                    encode_chunk(&mut body, 0x2004, &chunk.bytes);
                    count += 1;
                }
            }
            for (layer, cel) in frame_cels {
                let mut chunk = Writer::default();
                chunk.u16(*layer);
                match cel {
                    TestCel::Image { x, y, w, h, pixels } | TestCel::RawImage { x, y, w, h, pixels } => {
                        chunk.i16(*x);
                        chunk.i16(*y);
                        chunk.u8(255);
                        chunk.u16(match cel {
                            TestCel::Image { .. } => 2,
                            _ => 0,
                        });
                        chunk.i16(0);
                        chunk.raw(&[0; 5]);
                        chunk.u16(*w);
                        chunk.u16(*h);
                        match cel {
                            TestCel::Image { .. } => {
                                let mut encoder =
                                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
                                use std::io::Write as _;
                                encoder.write_all(pixels).expect("fixture compresses");
                                chunk.raw(&encoder.finish().expect("fixture finishes"));
                            }
                            _ => chunk.raw(pixels),
                        }
                    }
                    TestCel::Linked { frame } => {
                        chunk.i16(0);
                        chunk.i16(0);
                        chunk.u8(255);
                        chunk.u16(1);
                        chunk.i16(0);
                        chunk.raw(&[0; 5]);
                        chunk.u16(*frame);
                    }
                }
                encode_chunk(&mut body, 0x2005, &chunk.bytes);
                count += 1;
            }
            if frame_index == 0 {
                for (kind, data) in frame0_chunks {
                    encode_chunk(&mut body, *kind, data);
                    count += 1;
                }
                if let Some(entries) = palette {
                    let mut chunk = Writer::default();
                    chunk.u32(entries.len() as u32);
                    chunk.u32(0);
                    chunk.u32(entries.len() as u32 - 1);
                    chunk.raw(&[0; 8]);
                    for [r, g, b, a] in entries {
                        chunk.u16(0);
                        chunk.u8(*r);
                        chunk.u8(*g);
                        chunk.u8(*b);
                        chunk.u8(*a);
                    }
                    encode_chunk(&mut body, 0x2019, &chunk.bytes);
                    count += 1;
                }
            }
            if let Some(extra) = other_chunks.get(frame_index) {
                for (kind, data) in extra {
                    encode_chunk(&mut body, *kind, data);
                    count += 1;
                }
            }
            frame_bodies.push((count, body.bytes));
        }

        let mut file = Writer::default();
        let file_size = 128 + frame_bodies.iter().map(|(_, b)| b.len() + 16).sum::<usize>();
        file.u32(file_size as u32);
        file.u16(HEADER_MAGIC);
        file.u16(cels.len() as u16);
        file.u16(width);
        file.u16(height);
        file.u16(depth);
        file.u32(1 | 2); // Layer opacity and group blending are valid.
        file.u16(speed_ms);
        file.raw(&[0; 8]);
        file.u8(transparent_index);
        file.raw(&[0; 3]);
        file.u16(0);
        file.raw(&[0; 2]); // Pixel ratio.
        file.raw(&[0; 8]); // Grid.
        file.raw(&[0; 84]);
        for (index, (count, body)) in frame_bodies.into_iter().enumerate() {
            file.u32((body.len() + 16) as u32);
            file.u16(FRAME_MAGIC);
            file.u16(count.min(u32::from(u16::MAX)) as u16);
            file.u16(*durations.get(index).unwrap_or(&100));
            file.raw(&[0; 2]);
            file.u32(count);
            file.raw(&body);
        }
        file.bytes
    }

    pub(crate) fn encode_chunk(out: &mut Writer, kind: u16, data: &[u8]) {
        out.u32((data.len() + 6) as u32);
        out.u16(kind);
        out.raw(data);
    }

    /// Encoded frame-tags chunk body.
    pub(crate) fn tags_chunk(tags: &[TestTag]) -> Vec<u8> {
        let mut chunk = Writer::default();
        chunk.u16(tags.len() as u16);
        chunk.raw(&[0; 8]);
        for tag in tags {
            chunk.u16(tag.from);
            chunk.u16(tag.to);
            chunk.u8(tag.direction);
            chunk.u16(tag.repeat);
            chunk.raw(&[0; 6]);
            chunk.raw(&[0; 3]);
            chunk.u8(0);
            chunk.string(tag.name);
        }
        chunk.bytes
    }

    /// One slice key of a fixture: frame, bounds, optional 9-patch centre and pivot.
    pub(crate) type SliceKeySpec = (u32, i32, i32, u32, u32, Option<[i32; 4]>, Option<[i32; 2]>);

    /// Encoded slice chunk body with one key per entry: (frame, x, y, w, h, center, pivot).
    pub(crate) fn slice_chunk(name: &str, nine_patch: bool, pivot: bool, keys: &[SliceKeySpec]) -> Vec<u8> {
        let mut chunk = Writer::default();
        chunk.u32(keys.len() as u32);
        chunk.u32((nine_patch as u32) | ((pivot as u32) << 1));
        chunk.u32(0);
        chunk.string(name);
        for (frame, x, y, w, h, center, key_pivot) in keys {
            chunk.u32(*frame);
            chunk.i32(*x);
            chunk.i32(*y);
            chunk.u32(*w);
            chunk.u32(*h);
            if nine_patch {
                let c = center.expect("slice key centre");
                chunk.i32(c[0]);
                chunk.i32(c[1]);
                chunk.u32(c[2] as u32);
                chunk.u32(c[3] as u32);
            }
            if pivot {
                let p = key_pivot.expect("slice key pivot");
                chunk.i32(p[0]);
                chunk.i32(p[1]);
            }
        }
        chunk.bytes
    }

    /// The RGBA pixels of a frame as `(r,g,b,a)` per pixel, row by row.
    pub(crate) fn pixels(image: &RgbaImage) -> Vec<[u8; 4]> {
        let (chunks, rest) = image.rgba8.as_chunks::<4>();
        assert!(rest.is_empty(), "the image buffer is not a multiple of 4 bytes");
        chunks.iter().map(|c| [c[0], c[1], c[2], c[3]]).collect()
    }

    #[test]
    fn rgba_frames_composite_layers_cels_and_durations() {
        // 4x2 canvas, two layers; the top layer moves and its third frame links back to the
        // first. Frame durations [50, 0 (=speed 80), 120].
        let bottom = [200u8, 10, 10, 255];
        let top = [30u8, 40, 250, 255];
        let cel = |x: i16| TestCel::Image {
            x,
            y: 0,
            w: 2,
            h: 2,
            pixels: top.repeat(4),
        };
        let file = build_file(&TestFile {
            width: 4,
            height: 2,
            depth: 32,
            transparent_index: 0,
            speed_ms: 80,
            layers: &[
                TestLayer {
                    name: "back",
                    visible: true,
                    opacity: 255,
                    blend: 0,
                },
                TestLayer {
                    name: "front",
                    visible: true,
                    opacity: 255,
                    blend: 0,
                },
            ],
            cels: &[
                vec![
                    (
                        0,
                        TestCel::Image {
                            x: 0,
                            y: 0,
                            w: 4,
                            h: 2,
                            pixels: bottom.repeat(8),
                        },
                    ),
                    (1, cel(2)),
                ],
                vec![
                    (
                        0,
                        TestCel::Image {
                            x: 0,
                            y: 0,
                            w: 4,
                            h: 2,
                            pixels: bottom.repeat(8),
                        },
                    ),
                    (1, cel(1)),
                ],
                vec![
                    (
                        0,
                        TestCel::Image {
                            x: 0,
                            y: 0,
                            w: 4,
                            h: 2,
                            pixels: bottom.repeat(8),
                        },
                    ),
                    (1, TestCel::Linked { frame: 0 }),
                ],
            ],
            durations: &[50, 0, 120],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "hero").expect("imports");
        assert_eq!(imported.canvas, [4, 2]);
        assert_eq!(imported.frames.len(), 3);
        assert_eq!(
            imported.frames.iter().map(|f| f.duration_ms).collect::<Vec<_>>(),
            vec![50, 80, 120],
            "a zero frame duration falls back to the header speed"
        );
        let f0 = pixels(&imported.frames[0].image);
        // Bottom layer is everywhere; the top cel at x=2..4 covers the right half.
        assert_eq!(f0[0], bottom, "left half is the bottom layer only");
        assert_eq!(f0[2], top, "right half is the top cel");
        assert_eq!(f0[4], bottom, "row 1 left");
        assert_eq!(f0[6], top, "row 1 right");
        let f2 = pixels(&imported.frames[2].image);
        // The link chunk is stored at (0, 0). It reuses frame 0's pixels but not that cel's x
        // (the source sits at x = 2). A link draws where its own cel header says.
        assert_eq!(f2[0], top, "linked cel draws at its own x");
        assert_eq!(f2[1], top, "linked cel draws at its own x");
        assert_eq!(f2[2], bottom, "the source cel's position is not copied");
        assert_eq!(f2[3], bottom, "the source cel's position is not copied");
        assert_eq!(f2[4], top);
        assert_eq!(f2[5], top);
        assert_eq!(f2[6], bottom);
        assert_eq!(f2[7], bottom);
    }

    #[test]
    fn hidden_and_blended_layers_are_skipped() {
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        let green = [0u8, 255, 0, 255];
        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[
                TestLayer {
                    name: "hidden",
                    visible: false,
                    opacity: 255,
                    blend: 0,
                },
                TestLayer {
                    name: "drawn",
                    visible: true,
                    opacity: 255,
                    blend: 0,
                },
            ],
            cels: &[vec![
                (
                    0,
                    TestCel::RawImage {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                        pixels: red.to_vec(),
                    },
                ),
                (
                    1,
                    TestCel::RawImage {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                        pixels: blue.to_vec(),
                    },
                ),
            ]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "s").expect("imports");
        assert_eq!(
            pixels(&imported.frames[0].image)[0],
            blue,
            "the hidden layer is skipped"
        );
        assert!(!imported.layers[0].visible);

        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "blended",
                visible: true,
                opacity: 255,
                blend: 1,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                    pixels: green.to_vec(),
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "s").expect("imports");
        assert_eq!(
            pixels(&imported.frames[0].image)[0],
            [0, 0, 0, 0],
            "a non-Normal blend layer is not composited"
        );
        assert!(imported.layers[0].blend_skipped);
        assert!(
            imported.warnings.iter().any(|w| w.contains("blend mode 1")),
            "the multiply layer warns: {imported:?}"
        );
    }

    #[test]
    fn reference_layers_are_skipped() {
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        // Build by hand so layer 0 carries the reference flag (64).
        let mut body = Writer::default();
        for (flags, name) in [(64u16, "ref"), (1u16, "drawn")] {
            let mut chunk = Writer::default();
            chunk.u16(flags);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u8(255);
            chunk.raw(&[0; 3]);
            chunk.string(name);
            encode_chunk(&mut body, 0x2004, &chunk.bytes);
        }
        let mut cel = Writer::default();
        cel.u16(0);
        cel.i16(0);
        cel.i16(0);
        cel.u8(255);
        cel.u16(0);
        cel.i16(0);
        cel.raw(&[0; 5]);
        cel.u16(1);
        cel.u16(1);
        cel.raw(&red);
        encode_chunk(&mut body, 0x2005, &cel.bytes);
        let mut cel2 = Writer::default();
        cel2.u16(1);
        cel2.i16(0);
        cel2.i16(0);
        cel2.u8(255);
        cel2.u16(0);
        cel2.i16(0);
        cel2.raw(&[0; 5]);
        cel2.u16(1);
        cel2.u16(1);
        cel2.raw(&blue);
        encode_chunk(&mut body, 0x2005, &cel2.bytes);

        let mut file = Writer::default();
        file.u32(128 + 16 + body.bytes.len() as u32);
        file.u16(HEADER_MAGIC);
        file.u16(1);
        file.u16(1);
        file.u16(1);
        file.u16(32);
        file.u32(1);
        file.u16(100);
        file.raw(&[0; 8]);
        file.u8(0);
        file.u16(0); // Color count.
        file.raw(&[0; 3 + 2 + 8 + 84]);
        file.u32(16 + body.bytes.len() as u32);
        file.u16(FRAME_MAGIC);
        file.u16(4);
        file.u16(100);
        file.raw(&[0; 2]);
        file.u32(4);
        file.raw(&body.bytes);
        let imported = import_aseprite(&file.bytes, "s").expect("imports");
        assert_eq!(pixels(&imported.frames[0].image)[0], blue);
        assert!(imported.layers[0].reference);
    }

    #[test]
    fn opacity_multiplies_through_cel_and_layer() {
        // Front layer at 50% over an opaque bottom layer: the pixel alpha 128 lands as
        // sa = 128 * (255*128) / 65025 = 64; oa = 64 + 255*(255-64)/255 = 255;
        // r = (250*64 + 10*255*(255-64)/255) / 255 = (16000 + 1910) / 255 = 70.
        let bottom = [10u8, 20, 30, 255];
        let top = [250u8, 240, 230, 128];
        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[
                TestLayer {
                    name: "back",
                    visible: true,
                    opacity: 255,
                    blend: 0,
                },
                TestLayer {
                    name: "front",
                    visible: true,
                    opacity: 128,
                    blend: 0,
                },
            ],
            cels: &[vec![
                (
                    0,
                    TestCel::RawImage {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                        pixels: bottom.to_vec(),
                    },
                ),
                (
                    1,
                    TestCel::RawImage {
                        x: 0,
                        y: 0,
                        w: 1,
                        h: 1,
                        pixels: top.to_vec(),
                    },
                ),
            ]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "s").expect("imports");
        let pixel = pixels(&imported.frames[0].image)[0];
        assert_eq!(pixel[0], 70);
        assert_eq!(pixel[3], 255);
    }

    /// Writes the same 2x1 gray-compatible picture (an opaque gray and a half-alpha gray) as
    /// RGBA, grayscale and indexed, and returns the three files.
    fn three_formats() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        // RGBA: (87,87,87,255) and (200,200,200,128).
        let rgba = build_file(&TestFile {
            width: 2,
            height: 1,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 1,
                    pixels: vec![87, 87, 87, 255, 200, 200, 200, 128],
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        // Grayscale: value 87 with alpha 255, value 200 with alpha 128.
        let gray = build_file(&TestFile {
            width: 2,
            height: 1,
            depth: 16,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 1,
                    pixels: vec![87, 255, 200, 128],
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        // Indexed: palette[1] = gray 87 opaque, palette[2] = gray 200 at half alpha.
        let palette: Vec<[u8; 4]> = vec![[0, 0, 0, 255], [87, 87, 87, 255], [200, 200, 200, 128]];
        let indexed = build_file(&TestFile {
            width: 2,
            height: 1,
            depth: 8,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 1,
                    pixels: vec![1, 2],
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: Some(&palette),
        });
        (rgba, gray, indexed)
    }

    #[test]
    fn indexed_grayscale_and_rgba_agree_on_a_test_image() {
        let (rgba, gray, indexed) = three_formats();
        // The same gray-compatible picture in three encodings: (87,87,87,255) and
        // (200,200,200,128).
        let from_rgba = import_aseprite(&rgba, "p").expect("rgba");
        let from_gray = import_aseprite(&gray, "p").expect("gray");
        let from_indexed = import_aseprite(&indexed, "p").expect("indexed");
        let a = pixels(&from_rgba.frames[0].image);
        let b = pixels(&from_gray.frames[0].image);
        let c = pixels(&from_indexed.frames[0].image);
        assert_eq!(a, b, "grayscale matches RGBA");
        assert_eq!(a, c, "indexed matches RGBA");
        assert_eq!(a[0], [87, 87, 87, 255]);
        assert_eq!(a[1], [200, 200, 200, 128]);
        assert_eq!(from_indexed.palette_size, 3);
    }

    #[test]
    fn transparent_index_is_transparent() {
        let palette: Vec<[u8; 4]> = vec![[9, 9, 9, 255], [1, 2, 3, 255]];
        let indexed = build_file(&TestFile {
            width: 2,
            height: 1,
            depth: 8,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 1,
                    pixels: vec![0, 1],
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: Some(&palette),
        });
        let imported = import_aseprite(&indexed, "p").expect("imports");
        assert_eq!(pixels(&imported.frames[0].image), vec![[0; 4], [1, 2, 3, 255]]);
    }

    #[test]
    fn tags_expand_into_clips_with_timing() {
        // Frames 0..4, durations 100 each; tags: walk 0..2 forward, back 2..4 reverse,
        // loop 0..4 ping-pong with repeat 3.
        let px = [7u8, 7, 7, 255];
        let cel = TestCel::Image {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            pixels: px.to_vec(),
        };
        let cels: Vec<Vec<(u16, TestCel)>> = (0..5).map(|_| vec![(0, cel.clone())]).collect();
        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &cels,
            durations: &[100, 100, 100, 100, 100],
            frame0_chunks: vec![(
                0x2018,
                tags_chunk(&[
                    TestTag {
                        name: "walk",
                        from: 0,
                        to: 2,
                        direction: 0,
                        repeat: 0,
                    },
                    TestTag {
                        name: "back",
                        from: 2,
                        to: 4,
                        direction: 1,
                        repeat: 0,
                    },
                    TestTag {
                        name: "loop",
                        from: 0,
                        to: 4,
                        direction: 2,
                        repeat: 3,
                    },
                ]),
            )],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "hero").expect("imports");
        assert_eq!(imported.tags.len(), 3);
        let clip = |name: &str| imported.clips.iter().find(|c| c.name == name).expect(name);
        assert_eq!(clip("walk").frames, vec![0, 1, 2]);
        assert_eq!(clip("back").frames, vec![4, 3, 2]);
        assert_eq!(clip("loop").frames, vec![0, 1, 2, 3, 4, 3, 2, 1]);
        assert_eq!(clip("loop").repeat, Some(3));
        assert_eq!(clip("loop").durations_ms, vec![100; 8]);
        assert_eq!(clip("walk").durations_ms.len(), 3);
        assert_eq!(imported.sprites.len(), 3);
        assert_eq!(imported.sprites[0].image.width, 3);
        assert_eq!(imported.sprites[2].image.width, 8);
    }

    #[test]
    fn slices_attach_nine_slice_and_pivot() {
        // 8x8 canvas, one tag covering frames 0..1; a 9-patch slice with a pivot.
        let px = [5u8; 4];
        let cel = TestCel::Image {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
            pixels: px.repeat(64),
        };
        let file = build_file(&TestFile {
            width: 8,
            height: 8,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(0, cel.clone())], vec![(0, cel)]],
            durations: &[100, 100],
            frame0_chunks: vec![
                (
                    0x2018,
                    tags_chunk(&[TestTag {
                        name: "panel",
                        from: 0,
                        to: 1,
                        direction: 0,
                        repeat: 0,
                    }]),
                ),
                (
                    0x2022,
                    slice_chunk(
                        "panel_slice",
                        true,
                        true,
                        &[(0, 0, 0, 8, 8, Some([2, 3, 4, 2]), Some([1, 7]))],
                    ),
                ),
            ],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "ui").expect("imports");
        assert_eq!(imported.sprites.len(), 1);
        let sprite = &imported.sprites[0];
        assert_eq!(sprite.name, "panel");
        let nine = sprite.nine_slice.expect("nine-slice");
        assert_eq!(
            (nine.left, nine.top, nine.right, nine.bottom),
            (2, 3, 8 - 2 - 4, 8 - 3 - 2),
            "borders are the centre rect's margins"
        );
        let pivot = sprite.pivot;
        assert!((pivot[0] - 1.0 / 8.0).abs() < 1e-6, "pivot x");
        assert!(
            (pivot[1] - (1.0 - 7.0 / 8.0)).abs() < 1e-6,
            "pivot y flips to +Y up"
        );
    }

    #[test]
    fn a_file_without_tags_gets_a_stem_named_default_clip() {
        let px = [1u8, 2, 3, 255];
        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                    pixels: px.to_vec(),
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "coin").expect("imports");
        assert_eq!(imported.clips.len(), 1);
        assert_eq!(imported.clips[0].name, "coin_default");
        assert_eq!(imported.sprites[0].name, "coin_default");
    }

    #[test]
    fn hostile_inputs_fail_with_documented_codes() {
        let px = [1u8, 2, 3, 255];
        let file = build_file(&TestFile {
            width: 2,
            height: 2,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2,
                    pixels: px.repeat(4),
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        // Truncations at every length fail, never panic.
        for cut in [0, 64, 127, 128, 140, 145, file.len() - 1] {
            let err = import_aseprite(&file[..cut], "s").expect_err("truncated");
            assert_eq!(err.code(), "aseprite_truncated", "cut {cut}");
        }
        // Bad magics.
        let mut bad = file.clone();
        bad[4] = 0x00;
        assert_eq!(
            import_aseprite(&bad, "s").expect_err("magic").code(),
            "aseprite_bad_magic"
        );
        // Unsupported depth.
        let mut bad_depth = file.clone();
        bad_depth[12] = 24;
        assert_eq!(
            import_aseprite(&bad_depth, "s").expect_err("depth").code(),
            "aseprite_unsupported_depth"
        );
        // A corrupt zlib stream in a compressed cel.
        let compressed = build_file(&TestFile {
            width: 2,
            height: 2,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::Image {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2,
                    pixels: px.repeat(4),
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        let mut bad_zlib = compressed.clone();
        let at = compressed.len() - 6;
        for b in &mut bad_zlib[at..] {
            *b = 0xAB;
        }
        let err = import_aseprite(&bad_zlib, "s").expect_err("zlib");
        assert_eq!(err.code(), "aseprite_zlib", "{err}");
    }

    #[test]
    fn z_index_reorders_cels() {
        // Two cels on one layer: default order would draw layer order; z-index +1 lifts the
        // first-drawn cel (layer 0) above layer 1.
        let red = [255u8, 0, 0, 255];
        let blue = [0u8, 0, 255, 255];
        let mut body = Writer::default();
        for name in ["bottom", "top"] {
            let mut chunk = Writer::default();
            chunk.u16(1);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u8(255);
            chunk.raw(&[0; 3]);
            chunk.string(name);
            encode_chunk(&mut body, 0x2004, &chunk.bytes);
        }
        // Cel on layer 0 with z-index +1 (drawn last).
        let mut cel = Writer::default();
        cel.u16(0);
        cel.i16(0);
        cel.i16(0);
        cel.u8(255);
        cel.u16(0);
        cel.i16(1);
        cel.raw(&[0; 5]);
        cel.u16(1);
        cel.u16(1);
        cel.raw(&red);
        encode_chunk(&mut body, 0x2005, &cel.bytes);
        let mut cel2 = Writer::default();
        cel2.u16(1);
        cel2.i16(0);
        cel2.i16(0);
        cel2.u8(255);
        cel2.u16(0);
        cel2.i16(0);
        cel2.raw(&[0; 5]);
        cel2.u16(1);
        cel2.u16(1);
        cel2.raw(&blue);
        encode_chunk(&mut body, 0x2005, &cel2.bytes);

        let mut file = Writer::default();
        file.u32(128 + 16 + body.bytes.len() as u32);
        file.u16(HEADER_MAGIC);
        file.u16(1);
        file.u16(1);
        file.u16(1);
        file.u16(32);
        file.u32(1);
        file.u16(100);
        file.raw(&[0; 8]);
        file.u8(0);
        file.u16(0); // Color count.
        file.raw(&[0; 3 + 2 + 8 + 84]);
        file.u32(16 + body.bytes.len() as u32);
        file.u16(FRAME_MAGIC);
        file.u16(4);
        file.u16(100);
        file.raw(&[0; 2]);
        file.u32(4);
        file.raw(&body.bytes);
        let imported = import_aseprite(&file.bytes, "s").expect("imports");
        assert_eq!(
            pixels(&imported.frames[0].image)[0],
            red,
            "the z-index +1 cel draws on top"
        );
    }

    #[test]
    fn group_opacity_multiplies_into_children() {
        // group at 50% containing a child layer at 50% over an opaque base: effective 25%:
        // sa = 255 * 128*128/65025 = 63 (cel 255 * layer-eff 64? -- compute: eff layer = 128*128/255 = 64).
        // sa = 255*64*255/65025... in code: alpha = cel_op(255) * eff(64) / 65025 * a(255) = 63.
        // channel r = (250*63 + 10*255*(255-63)/255) / (63 + 255*(255-63)/255 = 255) =
        // (15750 + 10*192) / 255 = 69.
        let base = [10u8, 0, 0, 255];
        let top = [250u8, 0, 0, 255];
        let mut body = Writer::default();
        for (kind, child, name) in [(0u16, 0u16, "base"), (1u16, 0u16, "group"), (0u16, 1u16, "child")] {
            let mut chunk = Writer::default();
            chunk.u16(1);
            chunk.u16(kind);
            chunk.u16(child);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u16(0);
            chunk.u8(if kind == 1 { 128 } else { 255 });
            chunk.raw(&[0; 3]);
            chunk.string(name);
            encode_chunk(&mut body, 0x2004, &chunk.bytes);
        }
        let mut cel = Writer::default();
        cel.u16(0);
        cel.i16(0);
        cel.i16(0);
        cel.u8(255);
        cel.u16(0);
        cel.i16(0);
        cel.raw(&[0; 5]);
        cel.u16(1);
        cel.u16(1);
        cel.raw(&base);
        encode_chunk(&mut body, 0x2005, &cel.bytes);
        let mut cel2 = Writer::default();
        cel2.u16(2);
        cel2.i16(0);
        cel2.i16(0);
        cel2.u8(255);
        cel2.u16(0);
        cel2.i16(0);
        cel2.raw(&[0; 5]);
        cel2.u16(1);
        cel2.u16(1);
        cel2.raw(&top);
        encode_chunk(&mut body, 0x2005, &cel2.bytes);

        let mut file = Writer::default();
        file.u32(128 + 16 + body.bytes.len() as u32);
        file.u16(HEADER_MAGIC);
        file.u16(1);
        file.u16(1);
        file.u16(1);
        file.u16(32);
        file.u32(1 | 2);
        file.u16(100);
        file.raw(&[0; 8]);
        file.u8(0);
        file.u16(0); // Color count.
        file.raw(&[0; 3 + 2 + 8 + 84]);
        file.u32(16 + body.bytes.len() as u32);
        file.u16(FRAME_MAGIC);
        file.u16(5);
        file.u16(100);
        file.raw(&[0; 2]);
        file.u32(5);
        file.raw(&body.bytes);
        let imported = import_aseprite(&file.bytes, "s").expect("imports");
        let pixel = pixels(&imported.frames[0].image)[0];
        assert_eq!(
            pixel[0], 130,
            "the group composites at 50% over the base: {pixel:?}"
        );
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn fixture_writer_round_trips_through_the_parser() {
        // The writer's own output must parse: a compressed cel with the palette, tags and a
        // slice (used by the cook-level acceptance test too).
        let _ = three_formats();
    }

    /// Old palette packets (0x0004): the count byte is the number of colors, and 0 means 256.
    /// `.max(256)` reads every packet as 256 colors, so a two-color palette fails to parse.
    #[test]
    fn old_palette_packet_count_is_the_color_count() {
        let mut pal = Writer::default();
        pal.u16(2);
        pal.u8(0);
        pal.u8(1);
        pal.raw(&[1, 2, 3]);
        pal.u8(0);
        pal.u8(1);
        pal.raw(&[4, 5, 6]);
        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 8,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                    pixels: vec![1],
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![(0x0004, pal.bytes)],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "idx").expect("old palette imports");
        assert_eq!(imported.palette_size, 256);
        assert_eq!(
            pixels(&imported.frames[0].image)[0],
            [4, 5, 6, 255],
            "index 1 is the second packet, not bytes read past the first"
        );
    }

    /// A count byte of 0 is a full 256-color packet (the field cannot store 256).
    #[test]
    fn old_palette_count_zero_means_256_colors() {
        let mut pal = Writer::default();
        pal.u16(1);
        pal.u8(0);
        pal.u8(0);
        let mut colors = vec![0u8; 256 * 3];
        colors[255 * 3] = 9;
        colors[255 * 3 + 1] = 8;
        colors[255 * 3 + 2] = 7;
        pal.raw(&colors);
        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 8,
            transparent_index: 0,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                    pixels: vec![255],
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![(0x0004, pal.bytes)],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "full").expect("256-color old palette");
        assert_eq!(pixels(&imported.frames[0].image)[0], [9, 8, 7, 255]);
    }

    /// Once a new palette (0x2019) has been read, a later old palette chunk is ignored, even
    /// when its packet table is truncated. Aseprite v1.1 writes both.
    #[test]
    fn old_palette_is_ignored_once_the_new_palette_is_read() {
        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 8,
            transparent_index: 2,
            speed_ms: 100,
            layers: &[TestLayer {
                name: "l",
                visible: true,
                opacity: 255,
                blend: 0,
            }],
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                    pixels: vec![0],
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![],
            // 65535 packets and no colors: parsing this chunk is `aseprite_truncated`.
            other_chunks: &[vec![(0x0004, vec![0xff, 0xff])]],
            palette: Some(&[[0, 255, 0, 255]]),
        });
        let imported = import_aseprite(&file, "both").expect("new palette wins");
        assert_eq!(pixels(&imported.frames[0].image)[0], [0, 255, 0, 255]);
    }

    /// A linked cel keeps the position and opacity in its own header. The linked frame supplies
    /// pixels only.
    #[test]
    fn linked_cel_keeps_its_own_position_and_opacity() {
        let red = [200u8, 0, 0, 255];
        let mut body = Writer::default();
        let mut layer = Writer::default();
        layer.u16(1);
        layer.u16(0);
        layer.u16(0);
        layer.u16(0);
        layer.u16(0);
        layer.u16(0);
        layer.u8(255);
        layer.raw(&[0; 3]);
        layer.string("l");
        encode_chunk(&mut body, 0x2004, &layer.bytes);
        let mut cel = Writer::default();
        cel.u16(0);
        cel.i16(0);
        cel.i16(0);
        cel.u8(255);
        cel.u16(0);
        cel.i16(0);
        cel.raw(&[0; 5]);
        cel.u16(1);
        cel.u16(1);
        cel.raw(&red);
        encode_chunk(&mut body, 0x2005, &cel.bytes);
        let frame0 = body.bytes.clone();

        let mut body1 = Writer::default();
        let mut link = Writer::default();
        link.u16(0);
        link.i16(1);
        link.i16(0);
        link.u8(128);
        link.u16(1);
        link.i16(0);
        link.raw(&[0; 5]);
        link.u16(0);
        encode_chunk(&mut body1, 0x2005, &link.bytes);

        let mut file = Writer::default();
        let file_size = 128 + (16 + frame0.len()) + (16 + body1.bytes.len());
        file.u32(file_size as u32);
        file.u16(HEADER_MAGIC);
        file.u16(2);
        file.u16(2);
        file.u16(1);
        file.u16(32);
        file.u32(1);
        file.u16(100);
        file.raw(&[0; 8]);
        file.u8(0);
        file.raw(&[0; 3]);
        file.u16(0);
        file.raw(&[0; 2]);
        file.raw(&[0; 8]);
        file.raw(&[0; 84]);
        for (count, body) in [(2u32, frame0), (1, body1.bytes)] {
            file.u32((body.len() + 16) as u32);
            file.u16(FRAME_MAGIC);
            file.u16(count as u16);
            file.u16(100);
            file.raw(&[0; 2]);
            file.u32(count);
            file.raw(&body);
        }
        let imported = import_aseprite(&file.bytes, "link").expect("imports");
        let frame = pixels(&imported.frames[1].image);
        assert_eq!(frame[0], [0, 0, 0, 0], "the link is not drawn at the source x");
        assert_eq!(
            frame[1],
            [200, 0, 0, 128],
            "the link's own x and opacity, source pixels: {frame:?}"
        );
    }

    /// Indexed transparent index clears non-background layers only.
    #[test]
    fn indexed_transparent_index_spares_the_background_layer() {
        let mut palette = Writer::default();
        palette.u32(1);
        palette.u32(0);
        palette.u32(0);
        palette.raw(&[0; 8]);
        palette.u16(0);
        palette.raw(&[255, 0, 0, 255]);

        let paint = |flags: u16| {
            let mut body = Writer::default();
            let mut layer = Writer::default();
            layer.u16(flags);
            layer.u16(0);
            layer.u16(0);
            layer.u16(0);
            layer.u16(0);
            layer.u16(0);
            layer.u8(255);
            layer.raw(&[0; 3]);
            layer.string("l");
            encode_chunk(&mut body, 0x2004, &layer.bytes);
            encode_chunk(&mut body, 0x2019, &palette.bytes);
            let mut cel = Writer::default();
            cel.u16(0);
            cel.i16(0);
            cel.i16(0);
            cel.u8(255);
            cel.u16(0);
            cel.i16(0);
            cel.raw(&[0; 5]);
            cel.u16(1);
            cel.u16(1);
            cel.u8(0);
            encode_chunk(&mut body, 0x2005, &cel.bytes);
            let mut file = Writer::default();
            file.u32(128 + 16 + body.bytes.len() as u32);
            file.u16(HEADER_MAGIC);
            file.u16(1);
            file.u16(1);
            file.u16(1);
            file.u16(8);
            file.u32(1);
            file.u16(100);
            file.raw(&[0; 8]);
            file.u8(0);
            file.raw(&[0; 3]);
            file.u16(0);
            file.raw(&[0; 2]);
            file.raw(&[0; 8]);
            file.raw(&[0; 84]);
            file.u32(16 + body.bytes.len() as u32);
            file.u16(FRAME_MAGIC);
            file.u16(3);
            file.u16(100);
            file.raw(&[0; 2]);
            file.u32(3);
            file.raw(&body.bytes);
            pixels(&import_aseprite(&file.bytes, "bg").expect("imports").frames[0].image)[0]
        };
        assert_eq!(
            paint(1 | 8),
            [255, 0, 0, 255],
            "a background layer keeps the transparent-index color"
        );
        assert_eq!(
            paint(1),
            [0, 0, 0, 0],
            "the same index is transparent on a normal layer"
        );
    }

    fn one_layer() -> [TestLayer; 1] {
        [TestLayer {
            name: "l",
            visible: true,
            opacity: 255,
            blend: 0,
        }]
    }

    /// Corr-low (pipeline-a #19): every bad cel used to add a warning, so a file of millions of
    /// small bad cels held (and wrote to `import_reports.json`) millions of lines. The list is
    /// capped at `MAX_WARNINGS`, with one closing line that counts the rest.
    #[test]
    fn corr_low_warnings_are_capped_with_a_count_of_the_rest() {
        let layers = one_layer();
        let bad = 5_000usize;
        let cels: Vec<Vec<(u16, TestCel)>> =
            vec![(0..bad).map(|_| (7u16, TestCel::Linked { frame: 0 })).collect()];
        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &layers,
            cels: &cels,
            durations: &[],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "flood").expect("bad cels are warnings");
        assert!(
            imported.warnings.len() <= MAX_WARNINGS + 1,
            "{} warnings kept",
            imported.warnings.len()
        );
        let last = imported.warnings.last().expect("a closing line");
        assert!(
            last.contains(&format!("{} more warnings", bad - MAX_WARNINGS)),
            "the closing line counts the dropped warnings: {last}"
        );
    }

    /// `.meta` settings get the `.atlas.ron` bounds: an unchecked `extrude` overflowed the
    /// packer's `width + 2 * extrude` (a panic in debug, an hours-long extrusion loop in release).
    #[test]
    fn meta_settings_out_of_range_are_rejected() {
        for text in [
            "(extrude: 2147483648)",
            "(extrude: 17)",
            "(padding: 4294967295)",
            "(max_size: 0)",
            "(max_size: 15)",
            "(max_size: 16385)",
        ] {
            let Err(err) = AsepriteImportSettings::try_from_meta_string(Some(text)) else {
                panic!("`{text}` must be rejected");
            };
            assert_eq!(err.code(), "aseprite_structure", "{text}: {err}");
        }
        let ok =
            AsepriteImportSettings::try_from_meta_string(Some("(max_size: 16, padding: 16, extrude: 16)"))
                .expect("the bounds themselves are valid");
        assert_eq!((ok.max_size, ok.padding, ok.extrude), (16, 16, 16));
        assert!(AsepriteImportSettings::try_from_meta_string(None).is_ok());
    }

    /// Every frame is composited into a kept canvas, so canvas × frames is bounded before any
    /// canvas is allocated (513 empty 1024² frames asked for 2 GiB and imported).
    #[test]
    fn composited_frames_over_the_pixel_budget_are_rejected() {
        let layers = one_layer();
        let cels: Vec<Vec<(u16, TestCel)>> = vec![Vec::new(); 513];
        let file = build_file(&TestFile {
            width: 1024,
            height: 1024,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &layers,
            cels: &cels,
            durations: &[],
            frame0_chunks: vec![(
                0x2018,
                tags_chunk(&[TestTag {
                    name: "one",
                    from: 0,
                    to: 0,
                    direction: 0,
                    repeat: 0,
                }]),
            )],
            other_chunks: &[],
            palette: None,
        });
        let Err(err) = import_aseprite(&file, "big") else {
            panic!("1024² × 513 frames is over 512 Mpx (the Ok value is not printed: it can hold gigabytes)");
        };
        assert_eq!(err.code(), "aseprite_limits", "{err}");
        // 512 frames is exactly the budget and still imports.
        let cels: Vec<Vec<(u16, TestCel)>> = vec![Vec::new(); 2];
        let file = build_file(&TestFile {
            width: 1024,
            height: 1024,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &layers,
            cels: &cels,
            durations: &[],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        assert!(import_aseprite(&file, "small").is_ok());
    }

    /// Linked cels reuse pixels they never paid for at parse time: 4096 links to one 1 Mpx cel
    /// is a ~100 KB file that composited 4.3 G pixels. The blend budget rejects it up front.
    #[test]
    fn linked_cels_over_the_composite_budget_are_rejected() {
        let layers = one_layer();
        let mut frame0 = vec![(
            0u16,
            TestCel::Image {
                x: 0,
                y: 0,
                w: 1024,
                h: 1024,
                pixels: vec![0; 1024 * 1024 * 4],
            },
        )];
        frame0.extend((0..4096).map(|_| (0u16, TestCel::Linked { frame: 0 })));
        let file = build_file(&TestFile {
            width: 1024,
            height: 1024,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &layers,
            cels: &[frame0],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        assert!(
            file.len() < 256 * 1024,
            "the hostile file is small: {} bytes",
            file.len()
        );
        let Err(err) = import_aseprite(&file, "links") else {
            panic!("4097 Mpx of blends is over budget (the Ok value is not printed: it can hold gigabytes)");
        };
        assert_eq!(err.code(), "aseprite_limits", "{err}");
    }

    /// Only the on-canvas part of a cel is visited (half-open cel-local ranges).
    #[test]
    fn canvas_overlap_clips_cels_to_the_canvas() {
        assert_eq!(canvas_overlap(0, 0, 4, 4, 8, 8), Some([0, 4, 0, 4]));
        assert_eq!(canvas_overlap(-2, 6, 4, 4, 8, 8), Some([2, 4, 0, 2]));
        assert_eq!(canvas_overlap(8, 0, 4, 4, 8, 8), None, "right of the canvas");
        assert_eq!(canvas_overlap(-4, 0, 4, 4, 8, 8), None, "left of the canvas");
        assert_eq!(
            canvas_overlap(-32768, -32768, 65535, 65535, 16384, 16384),
            Some([32768, 49152, 32768, 49152])
        );
    }

    /// An off-canvas cel draws nothing and a partly visible one draws its visible part.
    #[test]
    fn cels_partly_off_canvas_draw_only_their_visible_pixels() {
        let red = [255u8, 0, 0, 255];
        let file = build_file(&TestFile {
            width: 2,
            height: 1,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &one_layer(),
            cels: &[vec![
                (
                    0,
                    TestCel::RawImage {
                        x: -1,
                        y: 0,
                        w: 2,
                        h: 1,
                        pixels: [[0u8, 0, 255, 255], red].concat(),
                    },
                ),
                (
                    0,
                    TestCel::RawImage {
                        x: 5,
                        y: 0,
                        w: 1,
                        h: 1,
                        pixels: red.to_vec(),
                    },
                ),
            ]],
            durations: &[100],
            frame0_chunks: vec![],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "clip").expect("imports");
        assert_eq!(pixels(&imported.frames[0].image), vec![red, [0, 0, 0, 0]]);
    }

    /// The file stores the pivot relative to the slice origin and the 9-patch centre relative to
    /// the slice bounds; the sprite is the whole frame, so both are moved into frame coordinates
    /// (the old code divided by the slice size and dropped the slice origin).
    #[test]
    fn slice_pivot_and_nine_slice_are_in_frame_coordinates() {
        let file = build_file(&TestFile {
            width: 32,
            height: 32,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &one_layer(),
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                    pixels: vec![1, 2, 3, 255],
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![(
                0x2022,
                slice_chunk(
                    "s",
                    true,
                    true,
                    &[(0, 4, 8, 16, 16, Some([2, 3, 12, 10]), Some([8, 16]))],
                ),
            )],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "panel").expect("imports");
        let sprite = &imported.sprites[0];
        let nine = sprite.nine_slice.expect("nine-slice");
        assert_eq!(
            (nine.left, nine.top, nine.right, nine.bottom),
            (6, 11, 32 - 6 - 12, 32 - 11 - 10),
            "borders measured from the frame edges"
        );
        assert!(
            (sprite.pivot[0] - 12.0 / 32.0).abs() < 1e-6,
            "pivot x: {:?}",
            sprite.pivot
        );
        assert!(
            (sprite.pivot[1] - (1.0 - 24.0 / 32.0)).abs() < 1e-6,
            "pivot y: {:?}",
            sprite.pivot
        );
    }

    /// A reverse clip starts at its highest frame, so that frame's slice key decides.
    #[test]
    fn reverse_clip_uses_the_slice_key_of_its_first_played_frame() {
        let cel = TestCel::RawImage {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
            pixels: vec![1, 2, 3, 255],
        };
        let file = build_file(&TestFile {
            width: 4,
            height: 4,
            depth: 32,
            transparent_index: 0,
            speed_ms: 100,
            layers: &one_layer(),
            cels: &[vec![(0, cel.clone())], vec![(0, cel)]],
            durations: &[100, 100],
            frame0_chunks: vec![
                (
                    0x2018,
                    tags_chunk(&[TestTag {
                        name: "back",
                        from: 0,
                        to: 1,
                        direction: 1,
                        repeat: 0,
                    }]),
                ),
                (
                    0x2022,
                    slice_chunk(
                        "s",
                        false,
                        true,
                        &[
                            (0, 0, 0, 4, 4, None, Some([0, 0])),
                            (1, 0, 0, 4, 4, None, Some([1, 3])),
                        ],
                    ),
                ),
            ],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "rev").expect("imports");
        assert_eq!(imported.clips[0].frames, vec![1, 0]);
        assert_eq!(
            imported.sprites[0].pivot,
            [0.25, 0.25],
            "the frame 1 key (played first)"
        );
    }

    /// Chunk 0x0011 stores 6-bit channels: 63 is full intensity (255, not 252).
    #[test]
    fn old_six_bit_palette_scales_63_to_255() {
        let mut pal = Writer::default();
        pal.u16(1);
        pal.u8(0);
        pal.u8(2);
        pal.raw(&[63, 32, 0, 63, 63, 63]);
        let file = build_file(&TestFile {
            width: 1,
            height: 1,
            depth: 8,
            transparent_index: 9,
            speed_ms: 100,
            layers: &one_layer(),
            cels: &[vec![(
                0,
                TestCel::RawImage {
                    x: 0,
                    y: 0,
                    w: 1,
                    h: 1,
                    pixels: vec![0],
                },
            )]],
            durations: &[100],
            frame0_chunks: vec![(0x0011, pal.bytes)],
            other_chunks: &[],
            palette: None,
        });
        let imported = import_aseprite(&file, "six").expect("imports");
        assert_eq!(pixels(&imported.frames[0].image)[0], [255, 130, 0, 255]);
    }
}
