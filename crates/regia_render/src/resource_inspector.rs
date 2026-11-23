// SPDX-License-Identifier: MPL-2.0
//! GPU resource inspector: a registry of the renderer's persistent named targets
//! ([`list_resources`]) and a readback that turns one mip and layer of any of them into an
//! EXR or PNG file's bytes ([`dump_resource`]). The agent protocol exposes both as
//! `seeing.list_resources` and `seeing.dump_resource` (book `agents/resource-inspector.md`).
//!
//! **Listing is always on; dumping is a debug option.** [`track`] records a target's metadata
//! (label, format, size, mips, layers, bytes, [`MemoryCategory`], whether it can be copied) at
//! creation. It keeps the texture handle, which a dump needs, only while
//! [`set_debug_readback`] is on, because a held handle would keep a replaced (resized) target
//! alive. The same switch adds `COPY_SRC` to targets that do not already have it
//! ([`debug_usage`]), so a shipping device's usage flags are unchanged.
//!
//! An entry describes the target as it was last created under its label: a target the renderer
//! later released stays listed until the label is created again.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::exr_out::{self, ExrPlane, ExrSamples};
use crate::memory::{MemoryCategory, classify};
use crate::{RenderDevice, RenderError};

static DEBUG_READBACK: AtomicBool = AtomicBool::new(false);
static REGISTRY: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

/// Turns the agent's debug readback on or off. Call it before the renderer creates its
/// targets (the agent plugin does at startup): targets created while it is off cannot be
/// dumped, only listed.
pub fn set_debug_readback(on: bool) {
    DEBUG_READBACK.store(on, Ordering::Relaxed);
}

/// Whether debug readback is on ([`set_debug_readback`]).
#[must_use]
pub fn debug_readback() -> bool {
    DEBUG_READBACK.load(Ordering::Relaxed)
}

/// Extra usage flags a registered target needs: `COPY_SRC` while debug readback is on, nothing
/// otherwise. OR it into the usage of a persistent target at creation.
#[must_use]
pub fn debug_usage() -> wgpu::TextureUsages {
    if debug_readback() {
        wgpu::TextureUsages::COPY_SRC
    } else {
        wgpu::TextureUsages::empty()
    }
}

/// What kind of texture a resource is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    /// A 2D texture with one layer.
    Texture2d,
    /// A 2D texture array (or cube).
    Texture2dArray,
    /// A 3D texture.
    Texture3d,
    /// A 1D texture.
    Texture1d,
}

impl ResourceKind {
    /// Stable snake-case name (agent protocol).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Texture2d => "texture_2d",
            Self::Texture2dArray => "texture_2d_array",
            Self::Texture3d => "texture_3d",
            Self::Texture1d => "texture_1d",
        }
    }
}

/// Which file a dump writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DumpFormat {
    /// OpenEXR (float formats).
    Exr,
    /// PNG (8-bit formats).
    Png,
}

impl DumpFormat {
    /// File extension, also the protocol name (`exr`, `png`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Exr => "exr",
            Self::Png => "png",
        }
    }

    /// Parses a protocol name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "exr" => Some(Self::Exr),
            "png" => Some(Self::Png),
            _ => None,
        }
    }
}

/// One registered target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceInfo {
    /// The wgpu label (unique in the registry).
    pub label: String,
    /// Texture kind.
    pub kind: ResourceKind,
    /// The wgpu format, `Debug`-printed (`Rgba16Float`).
    pub format: String,
    /// Width of mip 0.
    pub width: u32,
    /// Height of mip 0.
    pub height: u32,
    /// Array layers (2D) or depth slices (3D); 1 for a plain 2D texture.
    pub layers: u32,
    /// Mip levels.
    pub mips: u32,
    /// MSAA sample count.
    pub samples: u32,
    /// Bytes of all mips, layers and samples.
    pub bytes: u64,
    /// The [`classify`] category of the label.
    pub category: MemoryCategory,
    /// A dump can read it now: it has `COPY_SRC`, one sample, a copyable format, and the
    /// registry holds its handle ([`set_debug_readback`]).
    pub copyable: bool,
    /// The file [`dump_resource`] writes for it, `None` when the format has no writer.
    pub dump_format: Option<DumpFormat>,
}

/// The names of `usage`'s flags joined by ` | ` (`RENDER_ATTACHMENT | TEXTURE_BINDING`; empty when
/// none is set).
#[must_use]
pub fn usage_names(usage: wgpu::TextureUsages) -> String {
    usage
        .iter_names()
        .map(|(name, _)| name)
        .collect::<Vec<_>>()
        .join(" | ")
}

struct Entry {
    info: ResourceInfo,
    /// The usage flags the texture was created with ([`resource_usage`]).
    usage: wgpu::TextureUsages,
    texture: Option<wgpu::Texture>,
}

/// How a texel format is read back and written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Half(u32),
    Float32(u32),
    /// `Rg11b10Ufloat`, widened to three FLOAT channels.
    Packed11,
    Unorm8 {
        channels: u32,
        bgra: bool,
    },
}

impl Layout {
    fn of(format: wgpu::TextureFormat) -> Option<Self> {
        use wgpu::TextureFormat as F;
        Some(match format {
            F::R16Float => Self::Half(1),
            F::Rg16Float => Self::Half(2),
            F::Rgba16Float => Self::Half(4),
            F::R32Float | F::Depth32Float => Self::Float32(1),
            F::Rg32Float => Self::Float32(2),
            F::Rgba32Float => Self::Float32(4),
            F::Rg11b10Ufloat => Self::Packed11,
            F::R8Unorm => Self::Unorm8 {
                channels: 1,
                bgra: false,
            },
            F::Rg8Unorm => Self::Unorm8 {
                channels: 2,
                bgra: false,
            },
            F::Rgba8Unorm => Self::Unorm8 {
                channels: 4,
                bgra: false,
            },
            F::Rgba8UnormSrgb => Self::Unorm8 {
                channels: 4,
                bgra: false,
            },
            F::Bgra8Unorm => Self::Unorm8 {
                channels: 4,
                bgra: true,
            },
            F::Bgra8UnormSrgb => Self::Unorm8 {
                channels: 4,
                bgra: true,
            },
            _ => return None,
        })
    }

    fn texel_bytes(self) -> u32 {
        match self {
            Self::Half(c) => 2 * c,
            Self::Float32(c) => 4 * c,
            Self::Packed11 => 4,
            Self::Unorm8 { channels, .. } => channels,
        }
    }

    fn dump_format(self) -> DumpFormat {
        match self {
            Self::Unorm8 { .. } => DumpFormat::Png,
            _ => DumpFormat::Exr,
        }
    }
}

/// Formats the GPU cannot copy out (or only through a depth/stencil aspect this writer skips).
fn gpu_copyable_format(format: wgpu::TextureFormat) -> bool {
    use wgpu::TextureFormat as F;
    !matches!(format, F::Depth24Plus | F::Depth24PlusStencil8 | F::Stencil8)
}

fn texture_bytes(texture: &wgpu::Texture) -> u64 {
    let size = texture.size();
    let (bw, bh) = texture.format().block_dimensions();
    let block = u64::from(texture.format().block_copy_size(None).unwrap_or(4));
    let layers = if texture.dimension() == wgpu::TextureDimension::D3 {
        1
    } else {
        u64::from(size.depth_or_array_layers.max(1))
    };
    let mut total = 0u64;
    for mip in 0..texture.mip_level_count().max(1) {
        let w = u64::from((size.width >> mip).max(1).div_ceil(bw));
        let h = u64::from((size.height >> mip).max(1).div_ceil(bh));
        let d = if texture.dimension() == wgpu::TextureDimension::D3 {
            u64::from((size.depth_or_array_layers >> mip).max(1))
        } else {
            1
        };
        total += w * h * d * block;
    }
    total * layers * u64::from(texture.sample_count().max(1))
}

fn info_of(label: &str, texture: &wgpu::Texture, held: bool) -> ResourceInfo {
    let size = texture.size();
    let format = texture.format();
    let layout = Layout::of(format);
    let kind = match texture.dimension() {
        wgpu::TextureDimension::D1 => ResourceKind::Texture1d,
        wgpu::TextureDimension::D3 => ResourceKind::Texture3d,
        wgpu::TextureDimension::D2 if size.depth_or_array_layers > 1 => ResourceKind::Texture2dArray,
        wgpu::TextureDimension::D2 => ResourceKind::Texture2d,
    };
    let copyable = held
        && texture.usage().contains(wgpu::TextureUsages::COPY_SRC)
        && texture.sample_count() == 1
        && gpu_copyable_format(format);
    ResourceInfo {
        label: label.to_owned(),
        kind,
        format: format!("{format:?}"),
        width: size.width,
        height: size.height,
        layers: size.depth_or_array_layers.max(1),
        mips: texture.mip_level_count().max(1),
        samples: texture.sample_count().max(1),
        bytes: texture_bytes(texture),
        category: classify(label),
        copyable,
        dump_format: layout.map(Layout::dump_format),
    }
}

fn registry() -> std::sync::MutexGuard<'static, Vec<Entry>> {
    // A poisoned lock only means a thread panicked while holding it; the list is still valid.
    REGISTRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Registers `texture` under `label`, replacing an earlier entry with the same label. Cheap
/// (one lock, a few field reads): call it where a persistent target is created.
pub fn track(label: &str, texture: &wgpu::Texture) {
    let held = debug_readback();
    let info = info_of(label, texture, held);
    let entry = Entry {
        info,
        usage: texture.usage(),
        texture: held.then(|| texture.clone()),
    };
    let mut registry = registry();
    match registry.iter_mut().find(|e| e.info.label == label) {
        Some(slot) => *slot = entry,
        None => registry.push(entry),
    }
}

/// Every registered target, sorted by label.
#[must_use]
pub fn list_resources() -> Vec<ResourceInfo> {
    let mut out: Vec<ResourceInfo> = registry().iter().map(|e| e.info.clone()).collect();
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// The usage flags of the target registered under `label` (`None`: no such target). A render
/// target is one with `RENDER_ATTACHMENT`; [`usage_names`] spells them out.
#[must_use]
pub fn resource_usage(label: &str) -> Option<wgpu::TextureUsages> {
    registry().iter().find(|e| e.info.label == label).map(|e| e.usage)
}

/// Why a dump failed. [`Self::code`] is the agent protocol's lowercase error code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InspectError {
    /// No target is registered under the label.
    NotFound(String),
    /// The target cannot be copied: no `COPY_SRC` (debug readback was off when it was created),
    /// MSAA without a resolve, or a depth format the GPU cannot copy (`Depth24Plus`).
    NotCopyable(String),
    /// The format has no writer, or the requested file format does not suit it.
    FormatUnsupported(String),
    /// The mip or layer is outside the target.
    OutOfRange(String),
    /// The copy or the map failed.
    Readback(String),
}

impl InspectError {
    /// The stable lowercase error code: `resource_not_found`, `resource_not_copyable`,
    /// `format_unsupported`, `subresource_out_of_range` or `readback_failed`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "resource_not_found",
            Self::NotCopyable(_) => "resource_not_copyable",
            Self::FormatUnsupported(_) => "format_unsupported",
            Self::OutOfRange(_) => "subresource_out_of_range",
            Self::Readback(_) => "readback_failed",
        }
    }

    /// The human-readable reason.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(m)
            | Self::NotCopyable(m)
            | Self::FormatUnsupported(m)
            | Self::OutOfRange(m)
            | Self::Readback(m) => m,
        }
    }
}

impl std::fmt::Display for InspectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

impl std::error::Error for InspectError {}

/// One mip and layer of a target, encoded as a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DumpedImage {
    /// The file's bytes.
    pub bytes: Vec<u8>,
    /// Which file format they are.
    pub format: DumpFormat,
    /// Width of the dumped mip.
    pub width: u32,
    /// Height of the dumped mip.
    pub height: u32,
}

/// The texture to read and its info, or why it cannot be read.
fn resolve(label: &str) -> Result<(wgpu::Texture, ResourceInfo), InspectError> {
    let registry = registry();
    let entry = registry
        .iter()
        .find(|e| e.info.label == label)
        .ok_or_else(|| InspectError::NotFound(format!("no resource labelled `{label}`")))?;
    let info = entry.info.clone();
    if info.samples > 1 {
        return Err(InspectError::NotCopyable(format!(
            "`{label}` is multisampled ({} samples) and has no resolve registered",
            info.samples
        )));
    }
    let Some(texture) = entry.texture.clone() else {
        // Without the handle the reason is the format when it cannot be copied at all, else the
        // debug-readback switch.
        let reason = if info.format.starts_with("Depth24Plus") || info.format == "Stencil8" {
            format!("`{label}` is {}: the GPU cannot copy it out", info.format)
        } else {
            format!(
                "`{label}` was created while debug readback was off; start the app with the agent's \
                 debug readback option"
            )
        };
        return Err(InspectError::NotCopyable(reason));
    };
    if !gpu_copyable_format(texture.format()) {
        return Err(InspectError::NotCopyable(format!(
            "`{label}` is {}: the GPU cannot copy it out",
            info.format
        )));
    }
    if !texture.usage().contains(wgpu::TextureUsages::COPY_SRC) {
        return Err(InspectError::NotCopyable(format!(
            "`{label}` has no COPY_SRC usage"
        )));
    }
    Ok((texture, info))
}

/// Reads mip `mip`, layer (or depth slice) `layer` of the target labelled `label` back from
/// the GPU and encodes it: EXR for float formats, PNG for 8-bit ones. `requested` (when given)
/// must be the format that suits the texture.
///
/// **Blocking, for tools and the agent protocol**: it submits a copy and waits for the GPU.
///
/// # Errors
///
/// [`InspectError`]: unknown label, a target the GPU cannot copy, a format without a writer
/// or a mismatched request, a mip or layer out of range, or a failed readback.
#[tracing::instrument(skip(device))]
pub fn dump_resource(
    device: &RenderDevice,
    label: &str,
    mip: u32,
    layer: u32,
    requested: Option<DumpFormat>,
) -> Result<DumpedImage, InspectError> {
    let (texture, info) = resolve(label)?;
    let format = texture.format();
    let layout = Layout::of(format).ok_or_else(|| {
        InspectError::FormatUnsupported(format!("`{label}` is {}: no writer for this format", info.format))
    })?;
    let want = layout.dump_format();
    if let Some(requested) = requested
        && requested != want
    {
        return Err(InspectError::FormatUnsupported(format!(
            "`{label}` is {}: write it as {}, not {}",
            info.format,
            want.name(),
            requested.name()
        )));
    }
    if mip >= info.mips {
        return Err(InspectError::OutOfRange(format!(
            "`{label}` has {} mips, asked for mip {mip}",
            info.mips
        )));
    }
    let depth_at_mip = if info.kind == ResourceKind::Texture3d {
        (info.layers >> mip).max(1)
    } else {
        info.layers
    };
    if layer >= depth_at_mip {
        return Err(InspectError::OutOfRange(format!(
            "`{label}` mip {mip} has {depth_at_mip} layers, asked for layer {layer}"
        )));
    }
    let width = (info.width >> mip).max(1);
    let height = (info.height >> mip).max(1);
    let raw = read_subresource(device, &texture, mip, layer, width, height, layout)?;
    let bytes = match layout {
        Layout::Unorm8 { channels, bgra } => encode_png(width, height, channels, bgra, &raw)?,
        _ => encode_exr(label, width, height, layout, &raw)?,
    };
    Ok(DumpedImage {
        bytes,
        format: want,
        width,
        height,
    })
}

/// One mip and layer of a target read back as floats: `channels` values per texel, row-major
/// from the top-left, `width * height * channels` in all ([`read_texels`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Texels {
    /// Width of the mip read.
    pub width: u32,
    /// Height of the mip read.
    pub height: u32,
    /// Values per texel: 1 (R), 2 (RG), 3 (RGB) or 4 (RGBA). BGRA formats are reordered to RGBA.
    pub channels: u32,
    /// The texel values: 8-bit unorm formats as `byte / 255` (the stored value; sRGB formats are
    /// not decoded), float formats as stored.
    pub values: Vec<f32>,
}

impl Texels {
    /// The value of channel `c` of texel (`x`, `y`) (`None` outside the image).
    #[must_use]
    pub fn get(&self, x: u32, y: u32, c: u32) -> Option<f32> {
        if x >= self.width || y >= self.height || c >= self.channels {
            return None;
        }
        let i = (y as usize * self.width as usize + x as usize) * self.channels as usize + c as usize;
        self.values.get(i).copied()
    }
}

/// Reads mip `mip`, layer (or depth slice) `layer` of the target labelled `label` back from the
/// GPU as floats ([`Texels`]): the editor's resource preview. The same copy as [`dump_resource`],
/// without the file encoding.
///
/// **Blocking**: it submits a copy and waits for the GPU; call it on a request, not every frame.
///
/// # Errors
///
/// [`InspectError`]: unknown label, a target the GPU cannot copy, a format without a reader, a
/// mip or layer out of range, or a failed readback.
#[tracing::instrument(skip(device))]
pub fn read_texels(device: &RenderDevice, label: &str, mip: u32, layer: u32) -> Result<Texels, InspectError> {
    let (texture, info) = resolve(label)?;
    let layout = Layout::of(texture.format()).ok_or_else(|| {
        InspectError::FormatUnsupported(format!("`{label}` is {}: no reader for this format", info.format))
    })?;
    if mip >= info.mips {
        return Err(InspectError::OutOfRange(format!(
            "`{label}` has {} mips, asked for mip {mip}",
            info.mips
        )));
    }
    let depth_at_mip = if info.kind == ResourceKind::Texture3d {
        (info.layers >> mip).max(1)
    } else {
        info.layers
    };
    if layer >= depth_at_mip {
        return Err(InspectError::OutOfRange(format!(
            "`{label}` mip {mip} has {depth_at_mip} layers, asked for layer {layer}"
        )));
    }
    let width = (info.width >> mip).max(1);
    let height = (info.height >> mip).max(1);
    let raw = read_subresource(device, &texture, mip, layer, width, height, layout)?;
    let (channels, values) = decode_texels(layout, &raw);
    Ok(Texels {
        width,
        height,
        channels,
        values,
    })
}

/// The read-back bytes of a `layout` subresource as floats: `(channels, values)`.
fn decode_texels(layout: Layout, raw: &[u8]) -> (u32, Vec<f32>) {
    match layout {
        Layout::Half(c) => {
            let (halves, _) = raw.as_chunks::<2>();
            (
                c,
                halves
                    .iter()
                    .map(|h| half_to_f32(u16::from_le_bytes(*h)))
                    .collect(),
            )
        }
        Layout::Float32(c) => {
            let (words, _) = raw.as_chunks::<4>();
            (c, words.iter().map(|w| f32::from_le_bytes(*w)).collect())
        }
        Layout::Packed11 => {
            let (words, _) = raw.as_chunks::<4>();
            let mut out = Vec::with_capacity(words.len() * 3);
            for w in words {
                let v = u32::from_le_bytes(*w);
                out.push(small_float(v & 0x7ff, 6));
                out.push(small_float((v >> 11) & 0x7ff, 6));
                out.push(small_float(v >> 22, 5));
            }
            (3, out)
        }
        Layout::Unorm8 { channels, bgra } => {
            let mut out: Vec<f32> = raw.iter().map(|b| f32::from(*b) / 255.0).collect();
            if bgra {
                let (pixels, _) = out.as_chunks_mut::<4>();
                for px in pixels {
                    px.swap(0, 2);
                }
            }
            (channels, out)
        }
    }
}

/// An IEEE 754 binary16 value widened to `f32`.
fn half_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exp = i32::from((bits >> 10) & 0x1f);
    let mant_bits = bits & 0x3ff;
    let mant = f32::from(mant_bits);
    sign * match exp {
        0 => mant / 1024.0 * 2f32.powi(-14),
        31 if mant_bits == 0 => f32::INFINITY,
        31 => f32::NAN,
        e => (1.0 + mant / 1024.0) * 2f32.powi(e - 15),
    }
}

fn read_subresource(
    device: &RenderDevice,
    texture: &wgpu::Texture,
    mip: u32,
    layer: u32,
    width: u32,
    height: u32,
    layout: Layout,
) -> Result<Vec<u8>, InspectError> {
    let readback = InspectError::Readback;
    let row = width * layout.texel_bytes();
    let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("resource inspector readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let aspect = if texture.format() == wgpu::TextureFormat::Depth32Float {
        wgpu::TextureAspect::DepthOnly
    } else {
        wgpu::TextureAspect::All
    };
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("resource inspector readback"),
        });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: mip,
            origin: wgpu::Origin3d { x: 0, y: 0, z: layer },
            aspect,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    crate::device::TrackedDevice::observe(&device.device)
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| readback(RenderError::Poll(e).to_string()))?;
    rx.recv()
        .map_err(|_| readback("map callback dropped".into()))?
        .map_err(|e| readback(e.to_string()))?;
    let mapped = buffer
        .slice(..)
        .get_mapped_range()
        .map_err(|e| readback(e.to_string()))?;
    let mut out = Vec::with_capacity(row as usize * height as usize);
    for y in 0..height as usize {
        let start = y * padded as usize;
        out.extend_from_slice(&mapped[start..start + row as usize]);
    }
    drop(mapped);
    buffer.unmap();
    Ok(out)
}

const CHANNEL_NAMES: [&str; 4] = ["R", "G", "B", "A"];

fn encode_exr(
    label: &str,
    width: u32,
    height: u32,
    layout: Layout,
    raw: &[u8],
) -> Result<Vec<u8>, InspectError> {
    let n = width as usize * height as usize;
    let failed = |e: exr_out::ExrError| InspectError::Readback(e.to_string());
    let mut planes: Vec<ExrPlane> = Vec::new();
    match layout {
        Layout::Half(c) => {
            let c = c as usize;
            for k in 0..c {
                let samples: Vec<u16> = (0..n)
                    .map(|i| u16::from_le_bytes([raw[(i * c + k) * 2], raw[(i * c + k) * 2 + 1]]))
                    .collect();
                planes.push(plane(c, k, ExrSamples::Half(samples)));
            }
        }
        Layout::Float32(c) => {
            let c = c as usize;
            for k in 0..c {
                let samples: Vec<u32> = (0..n)
                    .map(|i| {
                        let o = (i * c + k) * 4;
                        u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]])
                    })
                    .collect();
                planes.push(plane(c, k, ExrSamples::Float(samples)));
            }
        }
        Layout::Packed11 => {
            for k in 0..3 {
                let samples: Vec<u32> = (0..n)
                    .map(|i| {
                        let o = i * 4;
                        let v = u32::from_le_bytes([raw[o], raw[o + 1], raw[o + 2], raw[o + 3]]);
                        let f = match k {
                            0 => small_float(v & 0x7ff, 6),
                            1 => small_float((v >> 11) & 0x7ff, 6),
                            _ => small_float(v >> 22, 5),
                        };
                        f.to_bits()
                    })
                    .collect();
                planes.push(plane(3, k, ExrSamples::Float(samples)));
            }
        }
        Layout::Unorm8 { .. } => {
            return Err(InspectError::FormatUnsupported(
                "8-bit formats are written as PNG".into(),
            ));
        }
    }
    exr_out::encode(
        width,
        height,
        &planes,
        &[("regia.resource".into(), label.to_owned())],
    )
    .map_err(failed)
}

/// The plane for channel `k` of a `count`-channel texel: `Y` for one channel, else `R`, `G`, `B`, `A`.
fn plane(count: usize, k: usize, samples: ExrSamples) -> ExrPlane {
    let name = if count == 1 { "Y" } else { CHANNEL_NAMES[k.min(3)] };
    ExrPlane {
        name: name.to_owned(),
        samples,
    }
}

/// Unsigned float with `mant_bits` mantissa bits and a 5-bit exponent (the R11G11B10 channels).
fn small_float(bits: u32, mant_bits: u32) -> f32 {
    let exp = ((bits >> mant_bits) & 0x1f) as i32;
    let mant = (bits & ((1 << mant_bits) - 1)) as f32;
    let scale = (1u32 << mant_bits) as f32;
    match exp {
        0 => mant / scale * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + mant / scale) * 2f32.powi(e - 15),
    }
}

fn encode_png(
    width: u32,
    height: u32,
    channels: u32,
    bgra: bool,
    raw: &[u8],
) -> Result<Vec<u8>, InspectError> {
    let color = match channels {
        1 => png::ColorType::Grayscale,
        2 => png::ColorType::GrayscaleAlpha,
        _ => png::ColorType::Rgba,
    };
    let mut data = raw.to_vec();
    if bgra {
        let (pixels, _) = data.as_chunks_mut::<4>();
        for px in pixels {
            px.swap(0, 2);
        }
    }
    let failed = |e: png::EncodingError| InspectError::Readback(format!("png: {e}"));
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(failed)?;
        writer.write_image_data(&data).map_err(failed)?;
        writer.finish().map_err(failed)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_map_to_writers() {
        use wgpu::TextureFormat as F;
        assert_eq!(Layout::of(F::Rgba16Float), Some(Layout::Half(4)));
        assert_eq!(
            Layout::of(F::Depth32Float).map(Layout::dump_format),
            Some(DumpFormat::Exr)
        );
        assert_eq!(
            Layout::of(F::Rgba8UnormSrgb).map(Layout::dump_format),
            Some(DumpFormat::Png)
        );
        assert_eq!(Layout::of(F::Depth24Plus), None);
        assert!(!gpu_copyable_format(F::Depth24Plus));
        assert!(!gpu_copyable_format(F::Depth24PlusStencil8));
        assert!(gpu_copyable_format(F::Depth32Float));
    }

    #[test]
    fn error_codes_are_lowercase_ascii() {
        let all = [
            InspectError::NotFound(String::new()),
            InspectError::NotCopyable(String::new()),
            InspectError::FormatUnsupported(String::new()),
            InspectError::OutOfRange(String::new()),
            InspectError::Readback(String::new()),
        ];
        for e in all {
            assert!(
                e.code().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{}",
                e.code()
            );
        }
        assert_eq!(InspectError::NotFound(String::new()).code(), "resource_not_found");
        assert_eq!(
            InspectError::NotCopyable(String::new()).code(),
            "resource_not_copyable"
        );
        assert_eq!(
            InspectError::FormatUnsupported(String::new()).code(),
            "format_unsupported"
        );
    }

    #[test]
    fn dump_format_names_round_trip() {
        for f in [DumpFormat::Exr, DumpFormat::Png] {
            assert_eq!(DumpFormat::parse(f.name()), Some(f));
        }
        assert_eq!(DumpFormat::parse("tga"), None);
    }

    #[test]
    fn packed_floats_decode() {
        // 1.0 in an 11-bit float (exp 15, mantissa 0) and a 10-bit float.
        assert_eq!(small_float(15 << 6, 6), 1.0);
        assert_eq!(small_float(15 << 5, 5), 1.0);
        assert_eq!(small_float(0, 6), 0.0);
        assert!(small_float(31 << 6, 6).is_infinite());
    }

    #[test]
    fn texels_decode_every_layout() {
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xc000), -2.0);
        assert_eq!(half_to_f32(0), 0.0);
        assert!(half_to_f32(0x7c00).is_infinite());
        let (c, v) = decode_texels(Layout::Half(2), &[0x00, 0x3c, 0x00, 0x38]);
        assert_eq!((c, v), (2, vec![1.0, 0.5]));
        let (c, v) = decode_texels(Layout::Float32(1), &2.5f32.to_le_bytes());
        assert_eq!((c, v), (1, vec![2.5]));
        let (c, v) = decode_texels(
            Layout::Unorm8 {
                channels: 4,
                bgra: true,
            },
            &[0, 51, 255, 255],
        );
        assert_eq!((c, v), (4, vec![1.0, 0.2, 0.0, 1.0]));
        let one = (15u32 << 6) | ((15u32 << 6) << 11) | ((15u32 << 5) << 22);
        let (c, v) = decode_texels(Layout::Packed11, &one.to_le_bytes());
        assert_eq!((c, v), (3, vec![1.0, 1.0, 1.0]));
        let texels = Texels {
            width: 2,
            height: 1,
            channels: 1,
            values: vec![0.25, 0.75],
        };
        assert_eq!(texels.get(1, 0, 0), Some(0.75));
        assert_eq!(texels.get(2, 0, 0), None);
        assert_eq!(texels.get(0, 0, 1), None);
    }

    #[test]
    fn usage_names_join_the_flags() {
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        let names = usage_names(usage);
        assert!(
            names.contains("RENDER_ATTACHMENT") && names.contains("TEXTURE_BINDING"),
            "{names}"
        );
        assert_eq!(usage_names(wgpu::TextureUsages::empty()), "");
    }

    /// ADR-8670: the dump's label goes in the `regia.resource` header attribute; the legacy key
    /// is no longer written.
    #[test]
    fn exr_dump_names_the_resource_under_the_current_key() {
        let exr = encode_exr("gbuffer.depth", 1, 1, Layout::Float32(1), &2.5f32.to_le_bytes()).expect("exr");
        let has = |needle: &[u8]| exr.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"regia.resource\0string\0"));
        assert!(has(b"gbuffer.depth"));
        let legacy = format!("{}.resource", regia_core::brand::LEGACY_KEY_PREFIX);
        assert!(!has(legacy.as_bytes()));
    }

    #[test]
    fn png_swaps_bgra() {
        let png = encode_png(1, 1, 4, true, &[10, 20, 30, 255]).expect("png");
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().expect("info");
        let mut buf = vec![0; reader.output_buffer_size().expect("size")];
        reader.next_frame(&mut buf).expect("frame");
        assert_eq!(&buf[..4], &[30, 20, 10, 255]);
    }
}
