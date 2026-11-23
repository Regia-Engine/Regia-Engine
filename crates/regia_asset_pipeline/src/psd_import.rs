// SPDX-License-Identifier: MPL-2.0
//! Adobe Photoshop PSD import (pure Rust; `book/agents/mesh-image-import.md`, ADR-3244).
//!
//! A `.psd` file cooks like a `.png`: the importer decodes it to RGBA8 and the normal texture
//! path (usage, `max_size`, compression, mips) takes over. The container is Adobe's published
//! "Adobe Photoshop File Formats Specification": a 26-byte header, the color mode data and image
//! resource sections (skipped), the layer and mask section, and the merged image data.
//!
//! * **Default: the merged composite** (the image data section). 8 and 16 bits a channel, RGB
//!   and grayscale, raw, RLE (PackBits), and ZIP compression with and without prediction. 16-bit
//!   samples round to 8 bits (`(v * 255 + 32767) / 65535`). Alpha comes from the first channel
//!   after the color channels, but only when the file flags it as transparency (a negative layer
//!   count, the way Photoshop writes it; `alpha: always` and `alpha: never` override). A file
//!   saved without "Maximize Compatibility" can hold a placeholder composite: use `layer` then.
//! * **One layer** (`.meta` `layer: Some("Name")`): the pixels of the single top-level layer with
//!   that exact name (the Unicode name when stored, else the Pascal name), at the layer's own
//!   rectangle (the output is that rectangle's size, not the canvas). Limits: layer opacity,
//!   blend mode, layer masks, clipping and visibility are ignored; a layer with layer effects
//!   (`lfx2`, `lrFX`) is refused (`psd_layer_effects`, the effects would not be in its pixels),
//!   as are groups and empty layers (`psd_layer_empty`); a name that matches two top-level
//!   layers is `psd_layer_ambiguous`. Text and smart-object layers import as their stored raster.
//! * **Refused with a code:** PSB (version 2, `psd_psb`), bitmap, indexed, CMYK, multichannel,
//!   duotone and Lab modes (`psd_unsupported_mode`), 1-bit and 32-bit depths
//!   (`psd_unsupported_depth`).

use std::io::Read;

use serde::{Deserialize, Serialize};

use crate::mesh_convert::read_capped;

/// The importer name the cook records for PSD sources.
pub const PSD_IMPORTER: &str = "psd";

/// Caps on one PSD import (untrusted input).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PsdLimits {
    /// Largest file, in bytes (default 1 GiB).
    pub max_file_bytes: u64,
    /// Most pixels in the decoded image (default 64 Mi: 8192 by 8192).
    pub max_pixels: u64,
    /// Most layer records read (default 8,192).
    pub max_layers: usize,
}

impl Default for PsdLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1 << 30,
            max_pixels: 1 << 26,
            max_layers: 8192,
        }
    }
}

/// Where the alpha channel of the composite comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PsdAlpha {
    /// The channel after the color channels, when the file flags it as transparency.
    #[default]
    Auto,
    /// The channel after the color channels, whenever there is one.
    Always,
    /// Opaque.
    Never,
}

/// PSD importer settings (the `.meta` `settings` string, next to the texture keys).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PsdImportSettings {
    /// Import this top-level layer instead of the merged composite.
    pub layer: Option<String>,
    /// Where alpha comes from (default `auto`).
    pub alpha: PsdAlpha,
    /// Hard caps.
    pub limits: PsdLimits,
}

impl PsdImportSettings {
    /// Settings from the `.meta` `settings` string; blank is the defaults, texture keys are
    /// ignored, a malformed string is an error.
    pub fn try_from_meta_string(settings: Option<&str>) -> Result<Self, PsdError> {
        match settings.filter(|s| !s.trim().is_empty()) {
            Some(s) => ron::from_str::<Self>(s).map_err(|e| PsdError::Settings(e.to_string())),
            None => Ok(Self::default()),
        }
    }

    /// A stable string of the choices that change the output (part of the cook key).
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!("layer={:?};alpha={:?}", self.layer, self.alpha)
    }
}

/// Why a PSD import failed. [`PsdError::code`] is the agent-protocol error code.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PsdError {
    /// The file could not be read.
    #[error("PSD file read error: {0}")]
    Io(String),
    /// The file is larger than [`PsdLimits::max_file_bytes`].
    #[error("PSD file is {size} bytes, more than the limit of {limit}")]
    TooLarge {
        /// File size in bytes.
        size: u64,
        /// The limit in bytes.
        limit: u64,
    },
    /// A malformed file.
    #[error("PSD: {0}")]
    Parse(String),
    /// A size limit was exceeded.
    #[error("PSD import limit exceeded: {0}")]
    Limit(String),
    /// The `.meta` settings string is malformed.
    #[error("PSD importer settings: {0}")]
    Settings(String),
    /// A large-document (PSB) file.
    #[error("PSB (large document) files are not imported; save as PSD")]
    Psb,
    /// A color mode other than grayscale and RGB.
    #[error("PSD color mode {0} is not supported (grayscale and RGB only)")]
    UnsupportedMode(u16),
    /// A bit depth other than 8 and 16.
    #[error("PSD bit depth {0} is not supported (8 and 16 only)")]
    UnsupportedDepth(u16),
    /// A compression method the importer does not know.
    #[error("PSD compression method {0} is not supported")]
    UnsupportedCompression(u16),
    /// No top-level layer has the requested name.
    #[error("no top-level layer named {0:?}")]
    LayerNotFound(String),
    /// More than one top-level layer has the requested name.
    #[error("more than one top-level layer is named {0:?}")]
    LayerAmbiguous(String),
    /// The layer has layer effects, which are not in its pixels.
    #[error("layer {0:?} has layer effects; merge or rasterize them in Photoshop")]
    LayerEffects(String),
    /// The layer is a group or has no pixels.
    #[error("layer {0:?} is a group or has no pixels")]
    LayerEmpty(String),
}

impl PsdError {
    /// The agent-protocol error code.
    pub fn code(&self) -> &'static str {
        match self {
            PsdError::Io(_) => "psd_io",
            PsdError::TooLarge { .. } => "psd_too_large",
            PsdError::Parse(_) => "psd_parse",
            PsdError::Limit(_) => "psd_limit",
            PsdError::Settings(_) => "psd_bad_settings",
            PsdError::Psb => "psd_psb",
            PsdError::UnsupportedMode(_) => "psd_unsupported_mode",
            PsdError::UnsupportedDepth(_) => "psd_unsupported_depth",
            PsdError::UnsupportedCompression(_) => "psd_unsupported_compression",
            PsdError::LayerNotFound(_) => "psd_layer_not_found",
            PsdError::LayerAmbiguous(_) => "psd_layer_ambiguous",
            PsdError::LayerEffects(_) => "psd_layer_effects",
            PsdError::LayerEmpty(_) => "psd_layer_empty",
        }
    }
}

/// A decoded PSD image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsdImage {
    /// Width, pixels.
    pub width: u32,
    /// Height, pixels.
    pub height: u32,
    /// `width * height * 4` bytes, straight (not premultiplied) alpha, top row first.
    pub rgba8: Vec<u8>,
    /// Bits a channel in the file (8 or 16).
    pub depth: u16,
    /// The file is grayscale.
    pub grayscale: bool,
    /// The alpha channel was used.
    pub has_alpha: bool,
    /// Layer records in the file.
    pub layers: usize,
    /// `composite`, or the name of the imported layer.
    pub source: String,
}

fn truncated() -> PsdError {
    PsdError::Parse("the file ends early".into())
}

struct Cur<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], PsdError> {
        let end = self.p.checked_add(n).ok_or_else(truncated)?;
        let s = self.d.get(self.p..end).ok_or_else(truncated)?;
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, PsdError> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, PsdError> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn i16(&mut self) -> Result<i16, PsdError> {
        Ok(self.u16()? as i16)
    }
    fn u32(&mut self) -> Result<u32, PsdError> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i32(&mut self) -> Result<i32, PsdError> {
        Ok(self.u32()? as i32)
    }
    fn skip(&mut self, n: usize) -> Result<(), PsdError> {
        self.bytes(n).map(|_| ())
    }
    /// A `u32`-length section, as a sub-cursor.
    fn section(&mut self) -> Result<Cur<'a>, PsdError> {
        let n = self.u32()? as usize;
        Ok(Cur {
            d: self.bytes(n)?,
            p: 0,
        })
    }
    fn rest(&self) -> &'a [u8] {
        &self.d[self.p.min(self.d.len())..]
    }
}

struct Header {
    channels: usize,
    width: u32,
    height: u32,
    depth: u16,
    gray: bool,
}

fn read_header(cur: &mut Cur<'_>, limits: &PsdLimits) -> Result<Header, PsdError> {
    if cur
        .bytes(4)
        .map_err(|_| PsdError::Parse("not a PSD file".into()))?
        != b"8BPS"
    {
        return Err(PsdError::Parse("not a PSD file (no 8BPS signature)".into()));
    }
    match cur.u16()? {
        1 => {}
        2 => return Err(PsdError::Psb),
        v => return Err(PsdError::Parse(format!("unknown version {v}"))),
    }
    cur.skip(6)?;
    let channels = usize::from(cur.u16()?);
    let height = cur.u32()?;
    let width = cur.u32()?;
    let depth = cur.u16()?;
    let mode = cur.u16()?;
    if !matches!(depth, 8 | 16) {
        return Err(PsdError::UnsupportedDepth(depth));
    }
    let gray = match mode {
        1 => true,
        3 => false,
        m => return Err(PsdError::UnsupportedMode(m)),
    };
    let colors = if gray { 1 } else { 3 };
    if channels < colors || channels > 56 {
        return Err(PsdError::Parse(format!(
            "{channels} channels for a {colors}-channel mode"
        )));
    }
    if width == 0 || height == 0 {
        return Err(PsdError::Parse("the image is empty".into()));
    }
    check_pixels(width, height, limits)?;
    Ok(Header {
        channels,
        width,
        height,
        depth,
        gray,
    })
}

fn check_pixels(width: u32, height: u32, limits: &PsdLimits) -> Result<(), PsdError> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels > limits.max_pixels {
        return Err(PsdError::Limit(format!(
            "{width}x{height} is {pixels} pixels (limit {})",
            limits.max_pixels
        )));
    }
    Ok(())
}

/// PackBits: decodes one row to exactly `row_bytes` bytes.
fn unpack_row(src: &[u8], row_bytes: usize, out: &mut Vec<u8>) -> Result<(), PsdError> {
    let start = out.len();
    let mut i = 0;
    while out.len() - start < row_bytes {
        let n = *src
            .get(i)
            .ok_or_else(|| PsdError::Parse("an RLE row ends early".into()))? as i8;
        i += 1;
        if n >= 0 {
            let count = n as usize + 1;
            let run = src
                .get(i..i + count)
                .ok_or_else(|| PsdError::Parse("an RLE literal run ends early".into()))?;
            out.extend_from_slice(run);
            i += count;
        } else if n != -128 {
            let count = 1 - isize::from(n);
            let byte = *src
                .get(i)
                .ok_or_else(|| PsdError::Parse("an RLE repeat ends early".into()))?;
            i += 1;
            out.extend(std::iter::repeat_n(byte, count as usize));
        }
        if out.len() - start > row_bytes {
            return Err(PsdError::Parse("an RLE row is longer than the image".into()));
        }
    }
    Ok(())
}

/// Undoes the ZIP predictor (compression 3) over rows of `row_bytes` bytes.
fn unpredict(plane: &mut [u8], row_bytes: usize, depth: u16) {
    for row in plane.chunks_mut(row_bytes) {
        if depth == 16 {
            let mut prev = 0u16;
            for w in row.as_chunks_mut::<2>().0.iter_mut() {
                let v = u16::from_be_bytes([w[0], w[1]]).wrapping_add(prev);
                w.copy_from_slice(&v.to_be_bytes());
                prev = v;
            }
        } else {
            let mut prev = 0u8;
            for b in row.iter_mut() {
                *b = b.wrapping_add(prev);
                prev = *b;
            }
        }
    }
}

/// Decodes the first `keep` of `planes` consecutive channel planes of `width` x `height` from
/// `data`, which starts with the compression method (the layout of both the image data section
/// and, one plane at a time, a layer channel). Planes past `keep` are skipped, not decoded, so
/// the memory used is bounded by `keep` planes.
fn decode_planes(
    data: &[u8],
    planes: usize,
    keep: usize,
    width: u32,
    height: u32,
    depth: u16,
) -> Result<Vec<Vec<u8>>, PsdError> {
    let mut cur = Cur { d: data, p: 0 };
    let method = cur.u16()?;
    let row_bytes = width as usize * usize::from(depth / 8);
    let plane_bytes = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| PsdError::Limit("plane too large".into()))?;
    let keep = keep.min(planes);
    let mut out = Vec::new();
    match method {
        0 => {
            for i in 0..planes {
                let bytes = cur.bytes(plane_bytes)?;
                if i < keep {
                    out.push(bytes.to_vec());
                }
            }
        }
        1 => {
            let rows = planes * height as usize;
            let mut counts = Vec::new();
            for _ in 0..rows {
                counts.push(usize::from(cur.u16()?));
            }
            let mut counts = counts.into_iter();
            for i in 0..planes {
                let mut plane = Vec::with_capacity(if i < keep { plane_bytes } else { 0 });
                for _ in 0..height {
                    let n = counts.next().ok_or_else(truncated)?;
                    let row = cur.bytes(n)?;
                    if i < keep {
                        unpack_row(row, row_bytes, &mut plane)?;
                    }
                }
                if i < keep {
                    out.push(plane);
                }
            }
        }
        2 | 3 => {
            let wanted = plane_bytes * keep;
            let mut all = Vec::new();
            flate2::read::ZlibDecoder::new(cur.rest())
                .take(wanted as u64)
                .read_to_end(&mut all)
                .map_err(|e| PsdError::Parse(format!("bad ZIP data: {e}")))?;
            if all.len() != wanted {
                return Err(PsdError::Parse(format!(
                    "ZIP data is {} bytes, expected at least {wanted}",
                    all.len()
                )));
            }
            for chunk in all.chunks(plane_bytes.max(1)).take(keep) {
                let mut plane = chunk.to_vec();
                if method == 3 {
                    unpredict(&mut plane, row_bytes.max(1), depth);
                }
                out.push(plane);
            }
        }
        m => return Err(PsdError::UnsupportedCompression(m)),
    }
    Ok(out)
}

/// One sample of a plane as 8 bits.
fn sample8(plane: &[u8], i: usize, depth: u16) -> u8 {
    if depth == 16 {
        let v = u32::from(u16::from_be_bytes([plane[2 * i], plane[2 * i + 1]]));
        ((v * 255 + 32767) / 65535) as u8
    } else {
        plane[i]
    }
}

fn to_rgba8(planes: &[&[u8]], alpha: Option<&[u8]>, gray: bool, pixels: usize, depth: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels * 4);
    for i in 0..pixels {
        let r = sample8(planes[0], i, depth);
        let (g, b) = if gray {
            (r, r)
        } else {
            (sample8(planes[1], i, depth), sample8(planes[2], i, depth))
        };
        let a = alpha.map_or(255, |p| sample8(p, i, depth));
        out.extend_from_slice(&[r, g, b, a]);
    }
    out
}

/// One layer record (what the importer needs of it).
struct Layer {
    name: String,
    rect: [i32; 4],
    /// `(channel id, data length)`.
    channels: Vec<(i16, usize)>,
    effects: bool,
    /// `lsct` section divider type: 1 or 2 open or closed folder, 3 the group's end marker.
    divider: u32,
}

fn pascal_name(cur: &mut Cur<'_>) -> Result<String, PsdError> {
    let n = usize::from(cur.u8()?);
    let bytes = cur.bytes(n)?;
    // The length byte and the text pad to a multiple of 4.
    let pad = (4 - (n + 1) % 4) % 4;
    cur.skip(pad.min(cur.rest().len()))?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn read_layer(cur: &mut Cur<'_>) -> Result<Layer, PsdError> {
    let top = cur.i32()?;
    let left = cur.i32()?;
    let bottom = cur.i32()?;
    let right = cur.i32()?;
    let n = usize::from(cur.u16()?);
    if n > 56 {
        return Err(PsdError::Parse(format!("a layer with {n} channels")));
    }
    let mut channels = Vec::with_capacity(n);
    for _ in 0..n {
        let id = cur.i16()?;
        channels.push((id, cur.u32()? as usize));
    }
    if cur.bytes(4)? != b"8BIM" {
        return Err(PsdError::Parse("a layer record lacks its blend signature".into()));
    }
    cur.skip(4 + 4)?; // blend key; opacity, clipping, flags, filler
    let mut extra = cur.section()?;
    // Layer mask data and blending ranges (each a length-prefixed block), then the name.
    extra.section()?;
    extra.section()?;
    let mut name = pascal_name(&mut extra)?;
    let mut effects = false;
    let mut divider = 0;
    // Tagged blocks. Their padding differs between writers (2 or 4), so the next signature
    // decides.
    let rest = extra.rest();
    let mut at = 0usize;
    while at + 12 <= rest.len() {
        let sig = &rest[at..at + 4];
        if sig != b"8BIM" && sig != b"8B64" {
            break;
        }
        let key = &rest[at + 4..at + 8];
        let len = u32::from_be_bytes([rest[at + 8], rest[at + 9], rest[at + 10], rest[at + 11]]) as usize;
        let start = at + 12;
        let Some(end) = start.checked_add(len).filter(|e| *e <= rest.len()) else {
            break;
        };
        let data = &rest[start..end];
        match key {
            b"luni" if data.len() >= 4 => {
                let count = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
                let units: Vec<u16> = data[4..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .take(count)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect();
                name = String::from_utf16_lossy(&units);
            }
            b"lsct" | b"lsdk" if data.len() >= 4 => {
                divider = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            }
            b"lfx2" | b"lrFX" => effects = true,
            _ => {}
        }
        let even = end + (end & 1);
        let four = (end + 3) & !3;
        at = [end, even, four]
            .into_iter()
            .find(|&c| rest.get(c..c + 4).is_some_and(|s| s == b"8BIM" || s == b"8B64"))
            .unwrap_or(end);
    }
    Ok(Layer {
        name,
        rect: [top, left, bottom, right],
        channels,
        effects,
        divider,
    })
}

/// Decodes the merged composite, or the named layer, of a PSD file.
pub fn decode_psd(bytes: &[u8], settings: &PsdImportSettings) -> Result<PsdImage, PsdError> {
    let _span = tracing::info_span!("psd_decode", bytes = bytes.len()).entered();
    let limits = &settings.limits;
    if bytes.len() as u64 > limits.max_file_bytes {
        return Err(PsdError::TooLarge {
            size: bytes.len() as u64,
            limit: limits.max_file_bytes,
        });
    }
    let mut cur = Cur { d: bytes, p: 0 };
    let header = read_header(&mut cur, limits)?;
    cur.section()?; // color mode data
    cur.section()?; // image resources
    let mut layer_mask = cur.section()?;
    let image_data = cur.rest();

    // Layer info: a length, the layer count (negative: the first alpha channel is transparency),
    // the records, then the channel data.
    let mut transparency = false;
    let mut layers: Vec<Layer> = Vec::new();
    let mut channel_data: &[u8] = &[];
    if !layer_mask.d.is_empty() {
        let mut info = layer_mask.section()?;
        if info.d.len() >= 2 {
            let count = info.i16()?;
            transparency = count < 0;
            let count = usize::from(count.unsigned_abs());
            if settings.layer.is_some() {
                if count > limits.max_layers {
                    return Err(PsdError::Limit(format!(
                        "{count} layers (limit {})",
                        limits.max_layers
                    )));
                }
                for _ in 0..count {
                    layers.push(read_layer(&mut info)?);
                }
                channel_data = info.rest();
            }
            // Without a `layer` setting only the count (the transparency flag) is needed.
        }
    }
    let layer_count = layers.len();

    if let Some(name) = &settings.layer {
        return decode_layer(name, &layers, channel_data, &header, settings);
    }

    let colors = if header.gray { 1 } else { 3 };
    let use_alpha = match settings.alpha {
        PsdAlpha::Auto => transparency && header.channels > colors,
        PsdAlpha::Always => header.channels > colors,
        PsdAlpha::Never => false,
    };
    // Only the color channels and, when used, the alpha channel are kept.
    let wanted = colors + usize::from(use_alpha);
    let planes = decode_planes(
        image_data,
        header.channels,
        wanted,
        header.width,
        header.height,
        header.depth,
    )?;
    let pixels = header.width as usize * header.height as usize;
    let refs: Vec<&[u8]> = planes.iter().map(Vec::as_slice).collect();
    let rgba8 = to_rgba8(
        &refs[..colors],
        use_alpha.then(|| refs[colors]),
        header.gray,
        pixels,
        header.depth,
    );
    Ok(PsdImage {
        width: header.width,
        height: header.height,
        rgba8,
        depth: header.depth,
        grayscale: header.gray,
        has_alpha: use_alpha,
        layers: layer_count,
        source: "composite".into(),
    })
}

fn decode_layer(
    name: &str,
    layers: &[Layer],
    mut channel_data: &[u8],
    header: &Header,
    settings: &PsdImportSettings,
) -> Result<PsdImage, PsdError> {
    // Walk in file order tracking group depth: a group's end marker (3) opens a nesting level,
    // its folder record (1 or 2) closes it.
    let mut depth = 0usize;
    let mut found: Vec<usize> = Vec::new();
    for (i, layer) in layers.iter().enumerate() {
        match layer.divider {
            3 => depth += 1,
            1 | 2 => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && layer.divider != 3 && layer.name == name {
            found.push(i);
        }
    }
    let index = match found.as_slice() {
        [] => return Err(PsdError::LayerNotFound(name.to_string())),
        [one] => *one,
        _ => return Err(PsdError::LayerAmbiguous(name.to_string())),
    };
    let layer = &layers[index];
    if matches!(layer.divider, 1 | 2) {
        return Err(PsdError::LayerEmpty(name.to_string()));
    }
    if layer.effects {
        return Err(PsdError::LayerEffects(name.to_string()));
    }
    let [top, left, bottom, right] = layer.rect;
    let (w, h) = (
        i64::from(right) - i64::from(left),
        i64::from(bottom) - i64::from(top),
    );
    if w <= 0 || h <= 0 || layer.channels.is_empty() {
        return Err(PsdError::LayerEmpty(name.to_string()));
    }
    let (w, h) = (
        u32::try_from(w).map_err(|_| PsdError::Limit("layer too wide".into()))?,
        u32::try_from(h).map_err(|_| PsdError::Limit("layer too tall".into()))?,
    );
    check_pixels(w, h, &settings.limits)?;
    // Skip the channel data of the layers before this one.
    let skip: usize = layers[..index]
        .iter()
        .flat_map(|l| l.channels.iter().map(|c| c.1))
        .sum();
    channel_data = channel_data.get(skip..).ok_or_else(truncated)?;
    let mut by_id: std::collections::BTreeMap<i16, Vec<u8>> = std::collections::BTreeMap::new();
    let mut at = 0usize;
    for &(id, len) in &layer.channels {
        let slice = channel_data
            .get(at..at.checked_add(len).ok_or_else(truncated)?)
            .ok_or_else(truncated)?;
        at += len;
        if matches!(id, -1..=2) {
            let mut planes = decode_planes(slice, 1, 1, w, h, header.depth)?;
            by_id.insert(id, planes.remove(0));
        }
    }
    let plane = |id: i16| {
        by_id
            .get(&id)
            .map(Vec::as_slice)
            .ok_or_else(|| PsdError::Parse(format!("layer {name:?} has no channel {id}")))
    };
    let colors: Vec<&[u8]> = if header.gray {
        vec![plane(0)?]
    } else {
        vec![plane(0)?, plane(1)?, plane(2)?]
    };
    let alpha = if settings.alpha == PsdAlpha::Never {
        None
    } else {
        by_id.get(&-1).map(Vec::as_slice)
    };
    let rgba8 = to_rgba8(&colors, alpha, header.gray, w as usize * h as usize, header.depth);
    Ok(PsdImage {
        width: w,
        height: h,
        rgba8,
        depth: header.depth,
        grayscale: header.gray,
        has_alpha: alpha.is_some(),
        layers: layers.len(),
        source: layer.name.clone(),
    })
}

/// Reads a `.psd` file within `limits.max_file_bytes`.
pub fn read_psd_file(path: &std::path::Path, limits: &PsdLimits) -> Result<Vec<u8>, PsdError> {
    match read_capped(path, limits.max_file_bytes) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(message)) => Err(PsdError::Io(message)),
        Err(size) => Err(PsdError::TooLarge {
            size,
            limit: limits.max_file_bytes,
        }),
    }
}

/// A minimal PSD writer for tests and the fuzz fixture (not an importer feature).
pub mod fixture {
    /// How a fixture stores channel data.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Compression {
        /// Method 0.
        Raw,
        /// Method 1 (PackBits, with runs).
        Rle,
        /// Method 2 (zlib).
        Zip,
        /// Method 3 (zlib with the row predictor).
        ZipPredict,
    }

    /// A fixture layer: its rectangle, name and RGBA8 pixels (`w * h * 4` bytes).
    #[derive(Clone, Debug)]
    pub struct Layer {
        /// The layer name.
        pub name: String,
        /// Left, top.
        pub origin: (i32, i32),
        /// Width, height.
        pub size: (u32, u32),
        /// RGBA8 pixels, top row first.
        pub rgba8: Vec<u8>,
        /// Write an `lfx2` effects block.
        pub effects: bool,
        /// `lsct` divider type (0 none).
        pub divider: u32,
    }

    /// What to write.
    #[derive(Clone, Debug)]
    pub struct Spec {
        /// Width, height of the canvas.
        pub size: (u32, u32),
        /// 8 or 16 bits a channel.
        pub depth: u16,
        /// Grayscale (one color channel) instead of RGB.
        pub gray: bool,
        /// Write the composite with an alpha channel and a negative layer count.
        pub alpha: bool,
        /// Channel data storage.
        pub compression: Compression,
        /// Composite pixels, RGBA8, top row first (`w * h * 4`; gray uses the red byte).
        pub rgba8: Vec<u8>,
        /// Layers, bottom first.
        pub layers: Vec<Layer>,
        /// Write version 2 (PSB).
        pub psb: bool,
    }

    fn packbits_row(row: &[u8], out: &mut Vec<u8>) {
        let mut i = 0;
        while i < row.len() {
            let mut run = 1;
            while i + run < row.len() && row[i + run] == row[i] && run < 128 {
                run += 1;
            }
            if run >= 2 {
                out.push((1 - run as i32) as i8 as u8);
                out.push(row[i]);
                i += run;
            } else {
                let start = i;
                while i < row.len() && i - start < 128 && !(i + 1 < row.len() && row[i + 1] == row[i]) {
                    i += 1;
                }
                if i == start {
                    i += 1;
                }
                out.push((i - start - 1) as u8);
                out.extend_from_slice(&row[start..i]);
            }
        }
    }

    /// One channel (or several, for the composite) as `compression` + data.
    fn encode(planes: &[Vec<u8>], width: u32, depth: u16, compression: Compression) -> Vec<u8> {
        use std::io::Write;
        let row_bytes = width as usize * usize::from(depth / 8);
        let mut out = Vec::new();
        match compression {
            Compression::Raw => {
                out.extend_from_slice(&0u16.to_be_bytes());
                for p in planes {
                    out.extend_from_slice(p);
                }
            }
            Compression::Rle => {
                out.extend_from_slice(&1u16.to_be_bytes());
                let mut rows: Vec<Vec<u8>> = Vec::new();
                for p in planes {
                    for row in p.chunks(row_bytes) {
                        let mut packed = Vec::new();
                        packbits_row(row, &mut packed);
                        rows.push(packed);
                    }
                }
                for r in &rows {
                    out.extend_from_slice(&(r.len() as u16).to_be_bytes());
                }
                for r in &rows {
                    out.extend_from_slice(r);
                }
            }
            Compression::Zip | Compression::ZipPredict => {
                let predict = compression == Compression::ZipPredict;
                out.extend_from_slice(&(if predict { 3u16 } else { 2u16 }).to_be_bytes());
                let mut raw = Vec::new();
                for p in planes {
                    if predict {
                        for row in p.chunks(row_bytes) {
                            if depth == 16 {
                                let mut prev = 0u16;
                                for w in row.as_chunks::<2>().0.iter() {
                                    let v = u16::from_be_bytes([w[0], w[1]]);
                                    raw.extend_from_slice(&v.wrapping_sub(prev).to_be_bytes());
                                    prev = v;
                                }
                            } else {
                                let mut prev = 0u8;
                                for &b in row {
                                    raw.push(b.wrapping_sub(prev));
                                    prev = b;
                                }
                            }
                        }
                    } else {
                        raw.extend_from_slice(p);
                    }
                }
                let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                let _ = z.write_all(&raw);
                out.extend_from_slice(&z.finish().unwrap_or_default());
            }
        }
        out
    }

    /// RGBA8 to one plane per channel in the file's depth (8-bit values widen by `* 257`).
    fn planes(rgba8: &[u8], count: usize, channels: &[usize], depth: u16) -> Vec<Vec<u8>> {
        channels
            .iter()
            .map(|&c| {
                let mut p = Vec::new();
                for px in rgba8.as_chunks::<4>().0.iter().take(count) {
                    let v = px[c];
                    if depth == 16 {
                        p.extend_from_slice(&(u16::from(v) * 257).to_be_bytes());
                    } else {
                        p.push(v);
                    }
                }
                p
            })
            .collect()
    }

    fn pascal(name: &str) -> Vec<u8> {
        let bytes = &name.as_bytes()[..name.len().min(255)];
        let mut out = vec![bytes.len() as u8];
        out.extend_from_slice(bytes);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }

    /// Writes the PSD.
    #[must_use]
    pub fn write(spec: &Spec) -> Vec<u8> {
        let (w, h) = spec.size;
        let colors: Vec<usize> = if spec.gray { vec![0] } else { vec![0, 1, 2] };
        let mut channels = colors.clone();
        if spec.alpha {
            channels.push(3);
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"8BPS");
        out.extend_from_slice(&(if spec.psb { 2u16 } else { 1u16 }).to_be_bytes());
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&(channels.len() as u16).to_be_bytes());
        out.extend_from_slice(&h.to_be_bytes());
        out.extend_from_slice(&w.to_be_bytes());
        out.extend_from_slice(&spec.depth.to_be_bytes());
        out.extend_from_slice(&(if spec.gray { 1u16 } else { 3u16 }).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // color mode data
        out.extend_from_slice(&0u32.to_be_bytes()); // image resources

        // Layer and mask information.
        let mut records = Vec::new();
        let mut data = Vec::new();
        for layer in &spec.layers {
            let (lw, lh) = layer.size;
            let ids: Vec<(i16, usize)> = if spec.gray {
                vec![(0, 0), (-1, 3)]
            } else {
                vec![(0, 0), (1, 1), (2, 2), (-1, 3)]
            };
            let count = (lw * lh) as usize;
            let mut encoded = Vec::new();
            for &(_, c) in &ids {
                encoded.push(encode(
                    &planes(&layer.rgba8, count, &[c], spec.depth),
                    lw,
                    spec.depth,
                    spec.compression,
                ));
            }
            records.extend_from_slice(&layer.origin.1.to_be_bytes());
            records.extend_from_slice(&layer.origin.0.to_be_bytes());
            records.extend_from_slice(&(layer.origin.1 + lh as i32).to_be_bytes());
            records.extend_from_slice(&(layer.origin.0 + lw as i32).to_be_bytes());
            records.extend_from_slice(&(ids.len() as u16).to_be_bytes());
            for ((id, _), e) in ids.iter().zip(&encoded) {
                records.extend_from_slice(&id.to_be_bytes());
                records.extend_from_slice(&(e.len() as u32).to_be_bytes());
            }
            records.extend_from_slice(b"8BIMnorm");
            records.extend_from_slice(&[255, 0, 0, 0]);
            let mut extra = Vec::new();
            extra.extend_from_slice(&0u32.to_be_bytes()); // mask
            extra.extend_from_slice(&0u32.to_be_bytes()); // blending ranges
            extra.extend_from_slice(&pascal(&layer.name));
            let mut block = |key: &[u8; 4], body: &[u8]| {
                extra.extend_from_slice(b"8BIM");
                extra.extend_from_slice(key);
                extra.extend_from_slice(&(body.len() as u32).to_be_bytes());
                extra.extend_from_slice(body);
                if body.len() % 2 == 1 {
                    extra.push(0);
                }
            };
            let units: Vec<u8> = layer.name.encode_utf16().flat_map(u16::to_be_bytes).collect();
            let mut luni = (layer.name.encode_utf16().count() as u32).to_be_bytes().to_vec();
            luni.extend_from_slice(&units);
            block(b"luni", &luni);
            if layer.divider != 0 {
                block(b"lsct", &layer.divider.to_be_bytes());
            }
            if layer.effects {
                block(b"lfx2", &[0, 0, 0, 0]);
            }
            records.extend_from_slice(&(extra.len() as u32).to_be_bytes());
            records.extend_from_slice(&extra);
            for e in encoded {
                data.extend_from_slice(&e);
            }
        }
        let mut info = Vec::new();
        let count = spec.layers.len() as i16;
        info.extend_from_slice(&(if spec.alpha { -count } else { count }).to_be_bytes());
        info.extend_from_slice(&records);
        info.extend_from_slice(&data);
        if info.len() % 2 == 1 {
            info.push(0);
        }
        let mut section = Vec::new();
        if spec.layers.is_empty() && !spec.alpha {
            // No layers: an empty layer and mask section.
        } else {
            section.extend_from_slice(&(info.len() as u32).to_be_bytes());
            section.extend_from_slice(&info);
            section.extend_from_slice(&0u32.to_be_bytes()); // global layer mask info
        }
        out.extend_from_slice(&(section.len() as u32).to_be_bytes());
        out.extend_from_slice(&section);

        // Merged image data.
        let count = (w * h) as usize;
        let composite = planes(&spec.rgba8, count, &channels, spec.depth);
        out.extend_from_slice(&encode(&composite, w, spec.depth, spec.compression));
        out
    }

    /// A 3 by 2 RGB composite with alpha and one layer, RLE: the fuzz fixture.
    #[must_use]
    pub fn small() -> Vec<u8> {
        let rgba8: Vec<u8> = (0..6u8)
            .flat_map(|i| [i * 40, 255 - i * 40, 7, 200 + i])
            .collect();
        write(&Spec {
            size: (3, 2),
            depth: 8,
            gray: false,
            alpha: true,
            compression: Compression::Rle,
            rgba8: rgba8.clone(),
            layers: vec![Layer {
                name: "Sprite".into(),
                origin: (0, 0),
                size: (3, 2),
                rgba8,
                effects: false,
                divider: 0,
            }],
            psb: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{Compression, Layer, Spec, write};
    use super::*;

    fn pixels(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let i = i as u8;
                [
                    i.wrapping_mul(7),
                    i.wrapping_mul(13).wrapping_add(5),
                    255 - i,
                    100 + (i % 100),
                ]
            })
            .collect()
    }

    fn spec(w: u32, h: u32) -> Spec {
        Spec {
            size: (w, h),
            depth: 8,
            gray: false,
            alpha: true,
            compression: Compression::Raw,
            rgba8: pixels(w, h),
            layers: vec![Layer {
                name: "Background".into(),
                origin: (0, 0),
                size: (1, 1),
                rgba8: vec![0; 4],
                effects: false,
                divider: 0,
            }],
            psb: false,
        }
    }

    #[test]
    fn error_codes_are_protocol_shaped() {
        let errors = [
            PsdError::Io(String::new()),
            PsdError::TooLarge { size: 0, limit: 0 },
            PsdError::Parse(String::new()),
            PsdError::Limit(String::new()),
            PsdError::Settings(String::new()),
            PsdError::Psb,
            PsdError::UnsupportedMode(0),
            PsdError::UnsupportedDepth(0),
            PsdError::UnsupportedCompression(0),
            PsdError::LayerNotFound(String::new()),
            PsdError::LayerAmbiguous(String::new()),
            PsdError::LayerEffects(String::new()),
            PsdError::LayerEmpty(String::new()),
        ];
        for error in errors {
            let code = error.code();
            assert!(code.starts_with("psd_"), "{code}");
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
    }

    #[test]
    fn every_compression_decodes_the_same_pixels() {
        for compression in [
            Compression::Raw,
            Compression::Rle,
            Compression::Zip,
            Compression::ZipPredict,
        ] {
            let mut s = spec(9, 5);
            s.compression = compression;
            let image = decode_psd(&write(&s), &PsdImportSettings::default()).expect("decode");
            assert_eq!((image.width, image.height), (9, 5));
            assert_eq!(image.rgba8, s.rgba8, "{compression:?}");
            assert!(image.has_alpha);
        }
    }

    #[test]
    fn sixteen_bit_and_gray_round_trip() {
        let mut s = spec(4, 3);
        s.depth = 16;
        let image = decode_psd(&write(&s), &PsdImportSettings::default()).expect("16-bit");
        assert_eq!(image.rgba8, s.rgba8);
        let mut g = spec(4, 3);
        g.gray = true;
        g.compression = Compression::Rle;
        let image = decode_psd(&write(&g), &PsdImportSettings::default()).expect("gray");
        for (got, want) in image.rgba8.chunks(4).zip(g.rgba8.chunks(4)) {
            assert_eq!(got, [want[0], want[0], want[0], want[3]]);
        }
    }

    #[test]
    fn alpha_follows_the_flag_unless_overridden() {
        let mut s = spec(2, 2);
        s.alpha = true;
        let bytes = write(&s);
        let never = PsdImportSettings {
            alpha: PsdAlpha::Never,
            ..PsdImportSettings::default()
        };
        let image = decode_psd(&bytes, &never).expect("never");
        assert!(image.rgba8.chunks(4).all(|p| p[3] == 255));
        s.alpha = false;
        let opaque = decode_psd(&write(&s), &PsdImportSettings::default()).expect("opaque");
        assert!(!opaque.has_alpha);
    }

    #[test]
    fn a_named_layer_imports_at_its_own_size() {
        let layer_pixels: Vec<u8> = (0..6u8).flat_map(|i| [i, i, i, 255 - i]).collect();
        let mut s = spec(8, 8);
        s.compression = Compression::Rle;
        let layer = |name: &str, effects: bool, divider: u32| Layer {
            name: name.into(),
            origin: (1, 1),
            size: (3, 2),
            rgba8: layer_pixels.clone(),
            effects,
            divider,
        };
        s.layers = vec![
            layer("Background", false, 0),
            layer("Glow", true, 0),
            layer("Sprite", false, 0),
        ];
        let bytes = write(&s);
        let pick = |name: &str| PsdImportSettings {
            layer: Some(name.into()),
            ..PsdImportSettings::default()
        };
        let image = decode_psd(&bytes, &pick("Sprite")).expect("layer");
        assert_eq!((image.width, image.height), (3, 2));
        assert_eq!(image.rgba8, layer_pixels);
        assert_eq!(image.source, "Sprite");
        assert_eq!(
            decode_psd(&bytes, &pick("Glow")).expect_err("fx").code(),
            "psd_layer_effects"
        );
        assert_eq!(
            decode_psd(&bytes, &pick("Nope")).expect_err("none").code(),
            "psd_layer_not_found"
        );
        // Groups: an inner layer is not top level; the folder itself has no pixels.
        s.layers = vec![
            layer("</Layer group>", false, 3),
            layer("Inner", false, 0),
            layer("Folder", false, 2),
            layer("Inner", false, 0),
        ];
        let bytes = write(&s);
        let image = decode_psd(&bytes, &pick("Inner")).expect("the top-level Inner");
        assert_eq!(image.rgba8, layer_pixels);
        assert_eq!(
            decode_psd(&bytes, &pick("Folder")).expect_err("group").code(),
            "psd_layer_empty"
        );
        s.layers = vec![layer("Twin", false, 0), layer("Twin", false, 0)];
        assert_eq!(
            decode_psd(&write(&s), &pick("Twin")).expect_err("two").code(),
            "psd_layer_ambiguous"
        );
    }

    #[test]
    fn refusals_carry_their_codes() {
        let s = PsdImportSettings::default();
        let mut psb = spec(2, 2);
        psb.psb = true;
        assert_eq!(decode_psd(&write(&psb), &s).expect_err("psb").code(), "psd_psb");
        let mut bytes = write(&spec(2, 2));
        bytes[24..26].copy_from_slice(&4u16.to_be_bytes()); // CMYK
        assert_eq!(
            decode_psd(&bytes, &s).expect_err("cmyk").code(),
            "psd_unsupported_mode"
        );
        let mut bytes = write(&spec(2, 2));
        bytes[22..24].copy_from_slice(&32u16.to_be_bytes());
        assert_eq!(
            decode_psd(&bytes, &s).expect_err("32").code(),
            "psd_unsupported_depth"
        );
        let mut bytes = write(&spec(2, 2));
        let at = bytes.len() - 2 * 2 * 4 - 2;
        bytes[at..at + 2].copy_from_slice(&9u16.to_be_bytes());
        assert_eq!(
            decode_psd(&bytes, &s).expect_err("method").code(),
            "psd_unsupported_compression"
        );
    }

    #[test]
    fn hostile_files_are_refused_without_a_panic() {
        let s = PsdImportSettings::default();
        let mut with_layers = spec(5, 4);
        with_layers.compression = Compression::Rle;
        with_layers.layers = vec![Layer {
            name: "L".into(),
            origin: (0, 0),
            size: (2, 2),
            rgba8: pixels(2, 2),
            effects: false,
            divider: 0,
        }];
        let by_layer = PsdImportSettings {
            layer: Some("L".into()),
            ..PsdImportSettings::default()
        };
        let bytes = write(&with_layers);
        for len in 0..bytes.len() {
            let _ = decode_psd(&bytes[..len], &s);
            let _ = decode_psd(&bytes[..len], &by_layer);
        }
        // A header claiming a huge canvas.
        let mut huge = write(&spec(2, 2));
        huge[14..18].copy_from_slice(&300_000u32.to_be_bytes());
        huge[18..22].copy_from_slice(&300_000u32.to_be_bytes());
        assert_eq!(decode_psd(&huge, &s).expect_err("huge").code(), "psd_limit");
        assert!(decode_psd(b"", &s).is_err());
        assert!(decode_psd(b"8BPS", &s).is_err());
    }
}
