// SPDX-License-Identifier: MPL-2.0
//! Material texture memory: the per-tier resident budget's eviction order, the per-texture
//! report ([`crate::FrameRenderer::texture_memory`], agent method `assets.texture_memory`) and
//! the loud diagnostics (`render.problems`, the host checklist) when a level loads too many
//! uncompressed textures or exceeds the budget.
//!
//! The budget itself is part of the streaming policy ([`crate::TextureStreaming::budget_mib`],
//! per tier from [`crate::TextureStreaming::for_preset`]); [`crate::texture_stream`] measures,
//! grows and shrinks each material's set. Eviction order: least recently seen first; among
//! materials seen equally recently, the one most over its screen-size need first.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::host_checklist::FrameProblem;

/// [`FrameProblem::code`] when the material textures resident exceed the texture budget.
pub const PROBLEM_TEXTURE_BUDGET: &str = "texture_budget_exceeded";

/// [`FrameProblem::code`] when more than [`UNCOMPRESSED_TEXTURE_LIMIT`] uncompressed (RGBA8)
/// material textures are resident: the level ships PNG/JPEG instead of cooked BC7/ASTC.
pub const PROBLEM_UNCOMPRESSED_TEXTURES: &str = "uncompressed_textures";

/// [`FrameProblem::code`] when block-compressed textures cooked for another platform reached a
/// device that cannot sample them: BCn on a phone without `TEXTURE_COMPRESSION_BC`, or ASTC /
/// ETC2 on a desktop GPU (decoded to RGBA8 on the CPU, at least 4× the block chain, about 9×
/// for ASTC 6×6, 16× for ASTC 8×8 and 36× for ASTC 12×12; ADR-2063), or a format
/// without a CPU decoder (left unbound). The content pack was cooked for the wrong platform
/// ([`report_wrong_platform_texture`]).
pub const PROBLEM_WRONG_PLATFORM_TEXTURES: &str = "textures_cooked_for_other_platform";

/// Uncompressed material textures a level may keep resident before
/// [`PROBLEM_UNCOMPRESSED_TEXTURES`] fires (UI and a handful of procedural maps are fine;
/// a level of PNGs is not).
pub const UNCOMPRESSED_TEXTURE_LIMIT: usize = 64;

/// A texture of at most this many texels (4 × 4) is a placeholder, not level content: it is
/// never counted against [`UNCOMPRESSED_TEXTURE_LIMIT`] nor named in the problem (v0.5.1: a
/// game's 122 4 × 4 placeholders raised it), and is reported as
/// [`TextureMemoryReport::tiny_uncompressed`] instead. 4 × 4, not 16 × 16 (ADR-7898): 8 to 16
/// texel maps are still real material textures to the level check.
pub const TINY_TEXTURE_TEXELS: u64 = 4 * 4;

/// Whether `entry` is uncompressed and not tiny ([`TINY_TEXTURE_TEXELS`]): what
/// [`TextureMemoryReport::uncompressed`] counts.
#[must_use]
pub fn counts_as_uncompressed(entry: &TextureMemoryEntry) -> bool {
    !entry.compressed && !is_tiny(entry)
}

/// Whether `entry` has at most [`TINY_TEXTURE_TEXELS`] texels.
#[must_use]
pub fn is_tiny(entry: &TextureMemoryEntry) -> bool {
    u64::from(entry.width) * u64::from(entry.height) <= TINY_TEXTURE_TEXELS
}

/// The budget the diagnostics check against when streaming is off (no resident budget):
/// Desktop High's.
pub const UNSTREAMED_WARN_BYTES: u64 = 2048 * 1024 * 1024;

/// Offenders a problem message names.
const NAMED_OFFENDERS: usize = 5;

/// One material set that may shrink, for [`plan_eviction`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EvictCandidate {
    /// Material key.
    pub key: u64,
    /// Last frame a visible draw used it.
    pub last_seen: u64,
    /// Resident edge over the edge its need asks for (`∞` when not needed this frame).
    pub excess: f32,
    /// Current skip (levels dropped from the top).
    pub skip: u32,
    /// Resident bytes at `skip`, `skip + 1`, … up to the deepest allowed skip.
    pub bytes_by_skip: Vec<u64>,
}

/// Which sets shrink, and to which skip, to free at least `need` bytes: least recently seen
/// first, then most over its need, then by key (a stable order); each takes the fewest levels
/// that cover what is left, or all it may give. Frees less than `need` only when every
/// candidate is at its limit.
#[must_use]
pub(crate) fn plan_eviction(mut candidates: Vec<EvictCandidate>, need: u64) -> Vec<(u64, u32)> {
    candidates.sort_by(|a, b| {
        a.last_seen
            .cmp(&b.last_seen)
            .then(b.excess.total_cmp(&a.excess))
            .then(a.key.cmp(&b.key))
    });
    let mut freed = 0u64;
    let mut plan = Vec::new();
    for c in candidates {
        if freed >= need {
            break;
        }
        let Some(&current) = c.bytes_by_skip.first() else {
            continue;
        };
        let left = need - freed;
        let deepest = c.bytes_by_skip.len() - 1;
        let pick = (1..=deepest)
            .find(|&i| current.saturating_sub(c.bytes_by_skip[i]) >= left)
            .unwrap_or(deepest);
        let gain = current.saturating_sub(c.bytes_by_skip[pick]);
        if pick == 0 || gain == 0 {
            continue;
        }
        freed += gain;
        plan.push((c.key, c.skip + pick as u32));
    }
    plan
}

/// GPU bytes of a `width`×`height` texture of `format` with `levels` mips.
#[must_use]
pub(crate) fn texture_bytes(format: wgpu::TextureFormat, width: u32, height: u32, levels: u32) -> u64 {
    let (bw, bh) = format.block_dimensions();
    let block = u64::from(format.block_copy_size(None).unwrap_or(4));
    (0..levels.max(1))
        .map(|l| {
            let (w, h) = ((width >> l).max(1), (height >> l).max(1));
            u64::from(w.div_ceil(bw)) * u64::from(h.div_ceil(bh)) * block
        })
        .sum()
}

/// `true` for formats the GPU stores block-compressed (BCn, ETC2, ASTC).
#[must_use]
pub(crate) fn is_compressed(format: wgpu::TextureFormat) -> bool {
    format.block_dimensions() != (1, 1)
}

// ---------------------------------------------------------------------------------------------
// Resident cap
// ---------------------------------------------------------------------------------------------

/// The largest edge any streamed texture can be resident at under the active tier (`0` = none,
/// [`CAP_UNSET`] = nothing published yet: the default preset's cap applies).
static RESIDENT_CAP: AtomicU32 = AtomicU32::new(CAP_UNSET);

/// [`RESIDENT_CAP`] before any policy was published.
const CAP_UNSET: u32 = u32::MAX;

/// Publishes `policy`'s cap ([`crate::TextureStreaming::max_px`] with streaming on, else none)
/// as the one materials are fitted to at load ([`resident_cap`]). The frame renderer publishes its
/// frame's policy every frame (with several renderers the last one wins); a host that loads its
/// level before the first frame publishes its tier's policy first
/// (`set_resident_cap(TextureStreaming::for_preset(preset))`).
pub fn set_resident_cap(policy: crate::TextureStreaming) {
    let cap = if policy.enabled { policy.max_px } else { 0 };
    RESIDENT_CAP.store(cap, Ordering::Relaxed);
}

/// The largest texture edge the active tier can ever make resident (`0` = the authored size):
/// pixels above it are never uploaded. [`crate::LabMaterial::from_authored_resolved`] and
/// [`crate::LabMaterial::from_cooked`] drop them at load (an 8 K map costs a 4 K one in RAM on
/// Desktop High, a 4 K map a 2 K one on Android); a host that decodes images itself can decode
/// at this size. Before any policy is published it is the default preset's (Desktop High, 4096).
#[must_use]
pub fn resident_cap() -> u32 {
    match RESIDENT_CAP.load(Ordering::Relaxed) {
        CAP_UNSET => crate::TextureStreaming::DESKTOP_HIGH.max_px,
        cap => cap,
    }
}

/// `texture` box-filtered down a level at a time until its largest edge is at most `cap`
/// (`srgb`: filtered in linear light). Unchanged when it fits or `cap` is 0.
#[must_use]
pub(crate) fn fit_texture(texture: crate::LabTexture, cap: u32, srgb: bool) -> crate::LabTexture {
    let (w, h) = (texture.width, texture.height);
    if cap == 0 || w.max(h) <= cap || texture.rgba.len() < (w as usize) * (h as usize) * 4 {
        return texture;
    }
    let _span = tracing::info_span!("fit_texture_to_cap").entered();
    crate::mips::mip_chain_rgba8(w, h, &texture.rgba, srgb)
        .into_iter()
        .find(|(lw, lh, _)| (*lw).max(*lh) <= cap)
        .map_or(texture, |(width, height, rgba)| crate::LabTexture {
            width,
            height,
            rgba,
        })
}

/// `texture` with the levels above `cap` dropped (no re-encode: the chain already has them).
#[must_use]
pub(crate) fn fit_compressed(
    mut texture: crate::compressed_texture::CompressedTexture,
    cap: u32,
) -> crate::compressed_texture::CompressedTexture {
    if cap == 0 {
        return texture;
    }
    let (bw, bh) = texture.format.block_dimensions();
    let mut drop = 0usize;
    // Keep level 0 in whole blocks (the texture is created at the new level-0 size).
    // The shifts saturate past the chain's end instead of panicking at 32+.
    while drop + 1 < texture.levels.len() {
        let w = texture.width.checked_shr(drop as u32).unwrap_or(0);
        let h = texture.height.checked_shr(drop as u32).unwrap_or(0);
        let wn = texture.width.checked_shr(drop as u32 + 1).unwrap_or(0);
        let hn = texture.height.checked_shr(drop as u32 + 1).unwrap_or(0);
        if w.max(h) <= cap || !wn.is_multiple_of(bw) || !hn.is_multiple_of(bh) || wn == 0 || hn == 0 {
            break;
        }
        drop += 1;
    }
    if drop > 0 {
        texture.levels.drain(..drop);
        texture.width >>= drop;
        texture.height >>= drop;
    }
    texture
}

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

fn names() -> &'static Mutex<HashMap<usize, Arc<str>>> {
    static NAMES: OnceLock<Mutex<HashMap<usize, Arc<str>>>> = OnceLock::new();
    NAMES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Entries the name table keeps before it starts over (names are diagnostics only).
const MAX_NAMES: usize = 1 << 16;

/// A cheap fingerprint of an image that survives clones and moves: its size, its byte length
/// and 32 words sampled evenly through the pixels (FNV-1a).
fn fingerprint(texture: &crate::LabTexture) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut mix = |v: u64| {
        hash ^= v;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    };
    mix(u64::from(texture.width));
    mix(u64::from(texture.height));
    mix(texture.rgba.len() as u64);
    let words = texture.rgba.len() / 4;
    for i in 0..32 {
        let at = (words * i / 32) * 4;
        if let Some(px) = texture.rgba.get(at..at + 4) {
            mix(u64::from(u32::from_le_bytes([px[0], px[1], px[2], px[3]])));
        }
    }
    hash as usize
}

/// The images (by [`fingerprint`]) or cooked chains (by `Arc` address) a material's
/// diagnostics name it by.
fn material_ids(material: &crate::LabMaterial) -> impl Iterator<Item = usize> + '_ {
    [
        &material.albedo,
        &material.normal_map,
        &material.occlusion_map,
        &material.emissive_map,
    ]
    .into_iter()
    .filter_map(|t| t.as_ref().filter(|t| !t.rgba.is_empty()).map(fingerprint))
    .chain(material.compressed.as_ref().map(|c| Arc::as_ptr(c) as usize))
}

/// Names `material`'s textures `name` in the texture memory report and the budget diagnostics
/// ([`crate::FrameRenderer::texture_memory`]). [`crate::LabMaterial::from_authored_resolved`]
/// and [`crate::LabMaterial::from_cooked`] call it with the asset name; a host that builds
/// materials itself may too. The name follows the material's images (by a sampled fingerprint,
/// so clones keep it) and its cooked chains (by address).
pub fn name_material_textures(material: &crate::LabMaterial, name: &str) {
    let Ok(mut table) = names().lock() else {
        return;
    };
    if table.len() >= MAX_NAMES {
        table.clear();
    }
    let name: Arc<str> = Arc::from(name);
    for id in material_ids(material) {
        table.insert(id, name.clone());
    }
}

/// The name [`name_material_textures`] gave the material holding `maps`, if any.
pub(crate) fn name_of_compressed(maps: &Arc<crate::compressed_texture::CompressedMaps>) -> Option<Arc<str>> {
    names().lock().ok()?.get(&(Arc::as_ptr(maps) as usize)).cloned()
}

/// The name [`name_material_textures`] gave `material`, if any.
pub(crate) fn material_name(material: &crate::LabMaterial) -> Option<Arc<str>> {
    let table = names().lock().ok()?;
    material_ids(material).find_map(|id| table.get(&id).cloned())
}

// ---------------------------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------------------------

/// One material texture the renderer holds.
#[derive(Clone, Debug, PartialEq)]
pub struct TextureMemoryEntry {
    /// The material's frame key ([`crate::FrameMesh::material_key`]).
    pub material_key: u64,
    /// The material's asset name, when known ([`name_material_textures`]).
    pub name: Option<String>,
    /// Slot: `base_color`, `normal`, `orm`, `emissive`, `detail`, `detail_normal`, `aux` or
    /// `graph<N>`.
    pub slot: &'static str,
    /// Authored size.
    pub width: u32,
    /// Authored size.
    pub height: u32,
    /// Size of the resident top level.
    pub resident_width: u32,
    /// Size of the resident top level.
    pub resident_height: u32,
    /// Texture format (`Rgba8UnormSrgb`, `Bc7RgbaUnormSrgb`, …).
    pub format: String,
    /// Block-compressed on the GPU.
    pub compressed: bool,
    /// Under the streaming budget (`false`: uploaded whole, outside it).
    pub streamed: bool,
    /// GPU bytes.
    pub gpu_bytes: u64,
    /// CPU bytes the renderer holds for it (`0` once uploaded at the residency it wants).
    pub cpu_bytes: u64,
    /// Last frame a visible draw used the material.
    pub last_seen_frame: u64,
}

/// Every material texture the renderer holds, the budget and the totals.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextureMemoryReport {
    /// Frame the report describes.
    pub frame: u64,
    /// Streaming on.
    pub streaming: bool,
    /// Resident budget of streamed maps (`u64::MAX`: none).
    pub budget_bytes: u64,
    /// GPU bytes of every material texture.
    pub resident_bytes: u64,
    /// Of which streamed (counted against the budget).
    pub streamed_bytes: u64,
    /// Of which uploaded whole (graph materials, detail/aux maps, streaming off).
    pub unstreamed_bytes: u64,
    /// CPU bytes the renderer holds for material textures (mip chains being uploaded).
    pub cpu_bytes: u64,
    /// Uncompressed (RGBA8) material textures, tiny ones ([`TINY_TEXTURE_TEXELS`]) not
    /// counted since v0.5.1.
    pub uncompressed: usize,
    /// GPU bytes of those.
    pub uncompressed_bytes: u64,
    /// v0.5.1: uncompressed textures of at most [`TINY_TEXTURE_TEXELS`] texels (placeholders,
    /// lookups): reported, never a problem.
    pub tiny_uncompressed: usize,
    /// v0.5.1: how many [`Self::uncompressed`] textures may be resident before
    /// [`PROBLEM_UNCOMPRESSED_TEXTURES`] fires; `None` is [`UNCOMPRESSED_TEXTURE_LIMIT`]
    /// ([`Self::uncompressed_limit`]).
    pub uncompressed_limit: Option<usize>,
    /// Every texture, largest first.
    pub textures: Vec<TextureMemoryEntry>,
    /// Textures cooked for another platform since the process started
    /// ([`wrong_platform_textures`]).
    pub wrong_platform: WrongPlatformTextures,
}

/// The totals of a [`TextureMemoryReport`] without its per-texture list
/// ([`crate::FrameRenderer::texture_memory_summary`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextureMemorySummary {
    /// Frame the report describes.
    pub frame: u64,
    /// Streaming on.
    pub streaming: bool,
    /// Resident budget of streamed maps (`u64::MAX`: none).
    pub budget_bytes: u64,
    /// [`TextureMemoryReport::checked_budget`].
    pub checked_budget: u64,
    /// GPU bytes of every material texture.
    pub resident_bytes: u64,
    /// Of which streamed.
    pub streamed_bytes: u64,
    /// Of which uploaded whole.
    pub unstreamed_bytes: u64,
    /// Material textures in the report.
    pub textures: usize,
}

impl TextureMemoryReport {
    /// The totals, without the per-texture list.
    #[must_use]
    pub fn summary(&self) -> TextureMemorySummary {
        TextureMemorySummary {
            frame: self.frame,
            streaming: self.streaming,
            budget_bytes: self.budget_bytes,
            checked_budget: self.checked_budget(),
            resident_bytes: self.resident_bytes,
            streamed_bytes: self.streamed_bytes,
            unstreamed_bytes: self.unstreamed_bytes,
            textures: self.textures.len(),
        }
    }

    /// The budget the diagnostics hold the frame to: the streaming budget, or
    /// [`UNSTREAMED_WARN_BYTES`] with streaming off.
    #[must_use]
    pub fn checked_budget(&self) -> u64 {
        if self.budget_bytes == u64::MAX {
            UNSTREAMED_WARN_BYTES
        } else {
            self.budget_bytes
        }
    }

    /// The uncompressed-texture limit in effect ([`Self::uncompressed_limit`] field or
    /// [`UNCOMPRESSED_TEXTURE_LIMIT`]).
    #[must_use]
    pub fn uncompressed_limit(&self) -> usize {
        self.uncompressed_limit.unwrap_or(UNCOMPRESSED_TEXTURE_LIMIT)
    }

    /// `true` when more [`Self::uncompressed`] textures are resident than
    /// [`Self::uncompressed_limit`] allows.
    #[must_use]
    pub fn over_uncompressed_limit(&self) -> bool {
        self.uncompressed > self.uncompressed_limit()
    }

    /// `true` when the resident material textures exceed [`Self::checked_budget`].
    #[must_use]
    pub fn over_budget(&self) -> bool {
        self.resident_bytes > self.checked_budget()
    }

    /// Sorts the entries largest first and fills the totals from them. Ties break by material,
    /// then slot, then name, then last-seen frame, then the remaining row fields (dimensions,
    /// format, streaming flags, CPU bytes), so two processes with the same resident set list
    /// equal rows in the same order (the agent report is comparable across runs). The last
    /// group matters for unnamed shared rows (texture views), which can tie on everything
    /// before them and would otherwise keep the `HashMap` iteration order they were pushed
    /// in (`sort_by` is stable).
    pub(crate) fn finish(&mut self) {
        self.textures.sort_by(|a, b| {
            b.gpu_bytes
                .cmp(&a.gpu_bytes)
                .then(a.material_key.cmp(&b.material_key))
                .then(a.slot.cmp(b.slot))
                .then(a.name.cmp(&b.name))
                .then(a.last_seen_frame.cmp(&b.last_seen_frame))
                .then(a.width.cmp(&b.width))
                .then(a.height.cmp(&b.height))
                .then(a.resident_width.cmp(&b.resident_width))
                .then(a.resident_height.cmp(&b.resident_height))
                .then(a.format.cmp(&b.format))
                .then(a.streamed.cmp(&b.streamed))
                .then(a.compressed.cmp(&b.compressed))
                .then(a.cpu_bytes.cmp(&b.cpu_bytes))
        });
        self.resident_bytes = self.textures.iter().map(|t| t.gpu_bytes).sum();
        self.streamed_bytes = self
            .textures
            .iter()
            .filter(|t| t.streamed)
            .map(|t| t.gpu_bytes)
            .sum();
        self.unstreamed_bytes = self.resident_bytes - self.streamed_bytes;
        self.cpu_bytes = self.textures.iter().map(|t| t.cpu_bytes).sum();
        self.uncompressed = self.textures.iter().filter(|t| counts_as_uncompressed(t)).count();
        self.uncompressed_bytes = self
            .textures
            .iter()
            .filter(|t| counts_as_uncompressed(t))
            .map(|t| t.gpu_bytes)
            .sum();
        self.tiny_uncompressed = self
            .textures
            .iter()
            .filter(|t| !t.compressed && is_tiny(t))
            .count();
    }

    /// The budget and uncompressed-texture problems this report shows (empty when neither).
    #[must_use]
    pub fn problems(&self) -> Vec<FrameProblem> {
        let mut out = Vec::new();
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        if self.over_budget() {
            out.push(FrameProblem {
                code: PROBLEM_TEXTURE_BUDGET,
                message: format!(
                    "material textures hold {:.0} MiB of GPU memory, over the {:.0} MiB texture budget{}; \
                     largest: {}",
                    mib(self.resident_bytes),
                    mib(self.checked_budget()),
                    if self.streaming {
                        " (what is left after eviction: floors, unstreamed graph/detail maps)"
                    } else {
                        " (texture streaming is off: turn on FrameQuality::texture_streaming)"
                    },
                    offenders(self.textures.iter())
                ),
            });
        }
        if self.over_uncompressed_limit() {
            out.push(FrameProblem {
                code: PROBLEM_UNCOMPRESSED_TEXTURES,
                message: format!(
                    "{} uncompressed (RGBA8) material textures resident ({:.0} MiB, limit {}): cook \
                     PNG/JPEG sources to BC7/BC5 (desktop) or ASTC (Android) with `regia cook` and \
                     load the pak; largest: {}",
                    self.uncompressed,
                    mib(self.uncompressed_bytes),
                    self.uncompressed_limit(),
                    offenders(self.textures.iter().filter(|t| counts_as_uncompressed(t)))
                ),
            });
        }
        if let Some(problem) = self.wrong_platform.problem() {
            out.push(problem);
        }
        out
    }
}

// ---------------------------------------------------------------------------------------------
// Textures cooked for another platform
// ---------------------------------------------------------------------------------------------

/// Block-compressed textures that reached a device which cannot sample their format: the
/// fallback a content pack cooked for the wrong platform takes (`regia cook --platform`
/// writes BCn for desktop and ASTC for Android). Process-wide, like [`resident_cap`]: the
/// renderer's material path and `regia_terrain`'s layer arrays report into it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WrongPlatformTextures {
    /// Textures decoded to RGBA8 on the CPU (BCn, ASTC LDR or ETC2 / EAC on a device without
    /// the format).
    pub decoded: usize,
    /// GPU bytes of their RGBA8 stand-ins.
    pub decoded_bytes: u64,
    /// Textures left unbound: no CPU decoder for the format (HDR ASTC, BC6H, signed EAC), or a
    /// terrain array of a format the device cannot sample.
    pub unbound: usize,
    /// The formats seen (`Debug` spelling), in first-seen order.
    pub formats: Vec<String>,
    /// The first five textures' labels.
    pub labels: Vec<String>,
}

impl WrongPlatformTextures {
    /// The [`PROBLEM_WRONG_PLATFORM_TEXTURES`] problem, when any texture took the fallback.
    #[must_use]
    pub fn problem(&self) -> Option<FrameProblem> {
        if self.decoded == 0 && self.unbound == 0 {
            return None;
        }
        let mut what = Vec::new();
        if self.decoded > 0 {
            what.push(format!(
                "{} decoded to RGBA8 on the CPU ({:.0} MiB; at least 4x the block chain, more when the block is larger than 4x4)",
                self.decoded,
                self.decoded_bytes as f64 / (1024.0 * 1024.0)
            ));
        }
        if self.unbound > 0 {
            what.push(format!("{} left unbound (no CPU decoder)", self.unbound));
        }
        Some(FrameProblem {
            code: PROBLEM_WRONG_PLATFORM_TEXTURES,
            message: format!(
                "textures cooked for another platform ({}): {}; cook the content for this device \
                 (`regia cook --platform android` writes ASTC, desktop cooks write BC); first: {}",
                self.formats.join(", "),
                what.join(", "),
                self.labels.join(", ")
            ),
        })
    }
}

#[derive(Default)]
struct WrongPlatformLog {
    seen: HashMap<u64, Option<u64>>,
    summary: WrongPlatformTextures,
}

fn wrong_platform_log() -> &'static Mutex<WrongPlatformLog> {
    static LOG: OnceLock<Mutex<WrongPlatformLog>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(WrongPlatformLog::default()))
}

/// Records that the texture `key` (any stable id: a blob's address, a hash of its label),
/// cooked as `format`, could not be sampled by this device: `decoded_bytes` is the size of its
/// RGBA8 stand-in, `None` when it was left unbound. The first texture of each format logs a
/// warning; every one counts towards [`PROBLEM_WRONG_PLATFORM_TEXTURES`]. A key reported again
/// (a reload) is counted once.
pub fn report_wrong_platform_texture(
    key: u64,
    label: &str,
    format: wgpu::TextureFormat,
    decoded_bytes: Option<u64>,
) {
    let Ok(mut log) = wrong_platform_log().lock() else {
        return;
    };
    if log.seen.contains_key(&key) {
        return;
    }
    log.seen.insert(key, decoded_bytes);
    let format_name = format!("{format:?}");
    if !log.summary.formats.contains(&format_name) {
        log.summary.formats.push(format_name);
        if decoded_bytes.is_some() {
            tracing::warn!(
                target: "regia_render",
                ?format,
                label,
                "the device cannot sample this block format: textures are decoded to RGBA8 on the \
                 CPU (at least 4x the block chain, more when the block is larger than 4x4; slow \
                 loads); the content was cooked for another platform \
                 (cook it with `regia cook --platform` for this device)"
            );
        } else {
            tracing::warn!(
                target: "regia_render",
                ?format,
                label,
                "the device cannot sample this block format and there is no CPU decoder for it: \
                 the texture is left unbound; the content was cooked for another platform"
            );
        }
    }
    match decoded_bytes {
        Some(bytes) => {
            log.summary.decoded += 1;
            log.summary.decoded_bytes += bytes;
        }
        None => log.summary.unbound += 1,
    }
    if log.summary.labels.len() < NAMED_OFFENDERS {
        log.summary.labels.push(format!("{label} ({format:?})"));
    }
}

/// Every texture [`report_wrong_platform_texture`] recorded since the process started.
#[must_use]
pub fn wrong_platform_textures() -> WrongPlatformTextures {
    wrong_platform_log()
        .lock()
        .map(|log| log.summary.clone())
        .unwrap_or_default()
}

/// The [`NAMED_OFFENDERS`] largest of `entries` (already largest first), one phrase each.
fn offenders<'a>(entries: impl Iterator<Item = &'a TextureMemoryEntry>) -> String {
    entries
        .take(NAMED_OFFENDERS)
        .map(|t| {
            format!(
                "{} {} {}×{} {} {:.1} MiB",
                t.name.as_deref().unwrap_or("(unnamed)"),
                t.slot,
                t.resident_width,
                t.resident_height,
                t.format,
                t.gpu_bytes as f64 / (1024.0 * 1024.0)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The render6 determinism audit: equal-size rows from one material (two shared slots of the
    /// same dimensions and format) used to keep the `HashMap` iteration order they were pushed
    /// in, so two processes listed them differently. The tie now breaks by slot, then name, then
    /// last-seen frame, in both insertion orders.
    #[test]
    fn finish_breaks_ties_by_slot() {
        fn row(slot: &'static str, name: &str, last_seen: u64) -> TextureMemoryEntry {
            TextureMemoryEntry {
                material_key: 7,
                name: Some(name.to_string()),
                slot,
                width: 1024,
                height: 1024,
                resident_width: 1024,
                resident_height: 1024,
                format: "Bc7RgbaUnormSrgb".to_string(),
                compressed: true,
                streamed: false,
                gpu_bytes: 1024,
                cpu_bytes: 0,
                last_seen_frame: last_seen,
            }
        }
        for order in [
            vec![row("normal", "oak", 9), row("base_color", "oak", 4)],
            vec![row("base_color", "oak", 4), row("normal", "oak", 9)],
        ] {
            let mut report = TextureMemoryReport {
                textures: order,
                ..TextureMemoryReport::default()
            };
            report.finish();
            let slots: Vec<&str> = report.textures.iter().map(|entry| entry.slot).collect();
            assert_eq!(slots, ["base_color", "normal"], "one order on every process");
        }
        // Same name, same last-seen frame: the slot key alone decides (rows that also tie on
        // the last-seen frame would keep the insertion order without it).
        for order in [
            vec![row("normal", "oak", 9), row("base_color", "oak", 9)],
            vec![row("base_color", "oak", 9), row("normal", "oak", 9)],
        ] {
            let mut report = TextureMemoryReport {
                textures: order,
                ..TextureMemoryReport::default()
            };
            report.finish();
            let slots: Vec<&str> = report.textures.iter().map(|entry| entry.slot).collect();
            assert_eq!(slots, ["base_color", "normal"], "the slot breaks the tie");
        }
    }

    /// Muse's read-only review of fix-render6 (finding 3): rows that tie on every key the
    /// report sorted by used to keep the `HashMap` iteration order they were pushed in
    /// (`sort_by` is stable), so two processes could still list them differently. Unnamed
    /// shared rows (texture views) share a slot and a format and can tie on everything but
    /// their dimensions; the remaining row fields break that tie.
    #[test]
    fn finish_breaks_dimension_ties_on_shared_rows() {
        fn row(width: u32, height: u32) -> TextureMemoryEntry {
            TextureMemoryEntry {
                material_key: 0,
                name: None,
                slot: "texture view",
                width,
                height,
                resident_width: width,
                resident_height: height,
                format: "Rgba8UnormSrgb".to_string(),
                compressed: false,
                streamed: false,
                gpu_bytes: 640 * 480 * 4,
                cpu_bytes: 0,
                last_seen_frame: 9,
            }
        }
        for order in [
            vec![row(640, 480), row(480, 640)],
            vec![row(480, 640), row(640, 480)],
        ] {
            let mut report = TextureMemoryReport {
                textures: order,
                ..TextureMemoryReport::default()
            };
            report.finish();
            let dims: Vec<(u32, u32)> = report
                .textures
                .iter()
                .map(|entry| (entry.width, entry.height))
                .collect();
            assert_eq!(dims, [(480, 640), (640, 480)], "one order on every process");
        }
    }

    fn cand(key: u64, last_seen: u64, excess: f32, bytes: &[u64]) -> EvictCandidate {
        EvictCandidate {
            key,
            last_seen,
            excess,
            skip: 0,
            bytes_by_skip: bytes.to_vec(),
        }
    }

    #[test]
    fn least_recently_seen_goes_first() {
        // A 4-level set: 100 → 25 → 6 → 1 bytes.
        let set = [100, 25, 6, 1];
        let plan = plan_eviction(
            vec![
                cand(1, 50, 1.0, &set),
                cand(2, 10, 1.0, &set),
                cand(3, 30, 1.0, &set),
            ],
            60,
        );
        // Key 2 (seen at frame 10) drops one level (75 bytes ≥ 60) and nothing else moves.
        assert_eq!(plan, vec![(2, 1)]);
        // More than one set can give: oldest first, each fully before the next.
        let plan = plan_eviction(
            vec![
                cand(1, 50, 1.0, &set),
                cand(2, 10, 1.0, &set),
                cand(3, 30, 1.0, &set),
            ],
            150,
        );
        assert_eq!(
            plan,
            vec![(2, 3), (3, 1)],
            "99 bytes from key 2, the other 51 from one level of key 3"
        );
    }

    #[test]
    fn equally_recent_sets_give_by_excess() {
        let set = [100, 25];
        let plan = plan_eviction(vec![cand(1, 9, 2.0, &set), cand(2, 9, 8.0, &set)], 50);
        assert_eq!(plan, vec![(2, 1)]);
        // An unneeded (∞ excess) set gives before a needed one.
        let plan = plan_eviction(vec![cand(1, 9, 2.0, &set), cand(2, 9, f32::INFINITY, &set)], 50);
        assert_eq!(plan, vec![(2, 1)]);
    }

    #[test]
    fn eviction_takes_the_fewest_levels_and_stops_at_limits() {
        let plan = plan_eviction(vec![cand(7, 0, 1.0, &[100, 25, 6, 1])], 80);
        assert_eq!(plan, vec![(7, 2)], "75 bytes is not enough, 94 is");
        // Everything given and still short: every candidate at its limit.
        let plan = plan_eviction(vec![cand(1, 0, 1.0, &[100, 25]), cand(2, 0, 1.0, &[10])], 1000);
        assert_eq!(plan, vec![(1, 1)]);
        assert!(plan_eviction(vec![cand(1, 0, 1.0, &[100, 25])], 0).is_empty());
    }

    #[test]
    fn bytes_count_blocks_and_mips() {
        // 4×4 RGBA8, 3 levels: 64 + 16 + 4.
        assert_eq!(texture_bytes(wgpu::TextureFormat::Rgba8Unorm, 4, 4, 3), 84);
        // BC7 is one 16-byte block per 4×4 (a 1×1 level still costs one block).
        assert_eq!(
            texture_bytes(wgpu::TextureFormat::Bc7RgbaUnorm, 8, 8, 4),
            64 + 16 + 16 + 16
        );
        // A 4 K RGBA8 map with its chain is ~85 MiB; BC7 is a quarter of that.
        let rgba = texture_bytes(wgpu::TextureFormat::Rgba8UnormSrgb, 4096, 4096, 13);
        let bc7 = texture_bytes(wgpu::TextureFormat::Bc7RgbaUnormSrgb, 4096, 4096, 13);
        // (The 1×1–2×2 levels cost a whole block, so slightly under 4×.)
        let ratio = rgba as f64 / bc7 as f64;
        assert!((ratio - 4.0).abs() < 0.01, "{ratio}");
        assert!(is_compressed(wgpu::TextureFormat::Bc5RgUnorm));
        assert!(!is_compressed(wgpu::TextureFormat::Rgba8Unorm));
    }

    fn entry(name: &str, bytes: u64, compressed: bool) -> TextureMemoryEntry {
        TextureMemoryEntry {
            material_key: bytes,
            name: Some(name.to_string()),
            slot: "base_color",
            width: 4096,
            height: 4096,
            resident_width: 4096,
            resident_height: 4096,
            format: if compressed {
                "Bc7RgbaUnormSrgb"
            } else {
                "Rgba8UnormSrgb"
            }
            .to_string(),
            compressed,
            streamed: true,
            gpu_bytes: bytes,
            cpu_bytes: 0,
            last_seen_frame: 0,
        }
    }

    #[test]
    fn wrong_platform_textures_are_counted_once_and_reported() {
        let empty = WrongPlatformTextures::default();
        assert!(empty.problem().is_none());
        // Keys unique to this test (the log is process-wide).
        let base = 0x5eed_0000_0000_0000u64;
        let before = wrong_platform_textures();
        report_wrong_platform_texture(
            base,
            "terrain layer albedo",
            wgpu::TextureFormat::Bc7RgbaUnormSrgb,
            Some(4096),
        );
        report_wrong_platform_texture(
            base,
            "terrain layer albedo",
            wgpu::TextureFormat::Bc7RgbaUnormSrgb,
            Some(4096),
        );
        report_wrong_platform_texture(
            base + 1,
            "hull normal",
            wgpu::TextureFormat::Astc {
                block: wgpu::AstcBlock::B4x4,
                channel: wgpu::AstcChannel::Unorm,
            },
            None,
        );
        let after = wrong_platform_textures();
        assert_eq!(
            after.decoded,
            before.decoded + 1,
            "a key reported twice counts once"
        );
        assert_eq!(after.decoded_bytes, before.decoded_bytes + 4096);
        assert_eq!(after.unbound, before.unbound + 1);
        assert!(after.formats.iter().any(|f| f.contains("Bc7RgbaUnormSrgb")));
        let report = TextureMemoryReport {
            wrong_platform: after,
            ..TextureMemoryReport::default()
        };
        let problems = report.problems();
        let problem = problems
            .iter()
            .find(|p| p.code == PROBLEM_WRONG_PLATFORM_TEXTURES)
            .expect("the wrong-platform problem");
        assert!(
            problem.message.contains("decoded to RGBA8"),
            "{}",
            problem.message
        );
        assert!(
            problem.message.contains("regia cook --platform"),
            "{}",
            problem.message
        );
        // An 8×8 ASTC block is 16 bytes and decodes to 256. The message must not claim a fixed 4–8×.
        report_wrong_platform_texture(
            base + 2,
            "astc 8x8",
            wgpu::TextureFormat::Astc {
                block: wgpu::AstcBlock::B8x8,
                channel: wgpu::AstcChannel::Unorm,
            },
            Some(256),
        );
        let astc = wrong_platform_textures()
            .problem()
            .expect("the 8x8 chain is decoded");
        assert!(
            !astc.message.contains("4-8") && !astc.message.contains("4–8"),
            "{}",
            astc.message
        );
    }

    /// v0.5.1 (DO27 tester B): 122 4 × 4 placeholders and 10 real uncompressed textures raise
    /// nothing; 70 real ones do. Placeholders are reported as `tiny_uncompressed`.
    #[test]
    fn tiny_uncompressed_placeholders_never_raise_the_problem() {
        let placeholder = |i: u64| TextureMemoryEntry {
            width: 4,
            height: 4,
            resident_width: 4,
            resident_height: 4,
            ..entry("placeholder", 64 + i, false)
        };
        let report_of = |real: u64| {
            let mut report = TextureMemoryReport {
                streaming: true,
                budget_bytes: u64::MAX - 1,
                textures: (0..122)
                    .map(placeholder)
                    .chain((0..real).map(|i| entry("png", (1 << 20) | i, false)))
                    .collect(),
                ..TextureMemoryReport::default()
            };
            report.finish();
            report
        };
        let ten = report_of(10);
        assert_eq!((ten.uncompressed, ten.tiny_uncompressed), (10, 122));
        assert!(ten.problems().is_empty(), "{:?}", ten.problems());
        let seventy = report_of(70);
        let problems = seventy.problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].code, PROBLEM_UNCOMPRESSED_TEXTURES);
        assert!(
            problems[0].message.starts_with("70 uncompressed"),
            "{}",
            problems[0].message
        );
        assert!(
            !problems[0].message.contains("placeholder"),
            "{}",
            problems[0].message
        );
        // The limit is the report's: a host may hold its level to a tighter one.
        let tight = TextureMemoryReport {
            uncompressed_limit: Some(8),
            ..ten.clone()
        };
        assert_eq!(tight.uncompressed_limit(), 8);
        assert_eq!(tight.problems().len(), 1);
        assert_eq!(ten.uncompressed_limit(), UNCOMPRESSED_TEXTURE_LIMIT);
        // 4 × 4 is still tiny, 5 × 4 and 8 × 8 are not.
        let edge = |w: u32, h: u32| TextureMemoryEntry {
            width: w,
            height: h,
            ..entry("edge", 1, false)
        };
        assert!(is_tiny(&edge(4, 4)) && is_tiny(&edge(1, 1)));
        assert!(!is_tiny(&edge(5, 4)) && !is_tiny(&edge(8, 8)));
    }

    #[test]
    fn problems_name_the_worst_offenders() {
        let mut report = TextureMemoryReport {
            streaming: true,
            budget_bytes: 1000,
            textures: vec![
                entry("small", 100, true),
                entry("hull_albedo", 900, false),
                entry("ok", 10, true),
            ],
            ..TextureMemoryReport::default()
        };
        report.finish();
        assert_eq!(report.resident_bytes, 1010);
        assert_eq!(report.uncompressed, 1);
        let problems = report.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].code, PROBLEM_TEXTURE_BUDGET);
        assert!(problems[0].message.starts_with("material textures hold"));
        let first = problems[0]
            .message
            .find("hull_albedo")
            .expect("names the largest");
        let second = problems[0].message.find("small").expect("names the next");
        assert!(first < second, "{}", problems[0].message);

        // Many uncompressed textures, inside the budget.
        let mut report = TextureMemoryReport {
            streaming: true,
            budget_bytes: u64::MAX - 1,
            textures: (0..=UNCOMPRESSED_TEXTURE_LIMIT as u64)
                .map(|i| entry("png", i + 1, false))
                .collect(),
            ..TextureMemoryReport::default()
        };
        report.finish();
        let problems = report.problems();
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].code, PROBLEM_UNCOMPRESSED_TEXTURES);
        assert!(problems[0].message.contains("regia cook"));

        // Streaming off: checked against Desktop High's budget.
        let report = TextureMemoryReport {
            budget_bytes: u64::MAX,
            resident_bytes: UNSTREAMED_WARN_BYTES + 1,
            ..TextureMemoryReport::default()
        };
        assert!(report.over_budget());
        assert!(report.problems()[0].message.contains("streaming is off"));
    }

    #[test]
    fn textures_fit_the_resident_cap() {
        let tex = crate::LabTexture {
            width: 8,
            height: 4,
            rgba: vec![200; 8 * 4 * 4],
        };
        let fitted = fit_texture(tex.clone(), 2, true);
        assert_eq!((fitted.width, fitted.height, fitted.rgba.len()), (2, 1, 8));
        assert_eq!(fit_texture(tex.clone(), 0, true).width, 8, "no cap");
        assert_eq!(fit_texture(tex.clone(), 8, false).width, 8, "fits");
        let chain = crate::compressed_texture::CompressedTexture {
            format: wgpu::TextureFormat::Bc7RgbaUnorm,
            width: 32,
            height: 32,
            levels: [1024, 256, 64, 16, 16, 16]
                .into_iter()
                .map(|n| vec![0; n].into())
                .collect(),
            normal_layout: crate::compressed_texture::NormalLayout::Xyz,
            mip_stream: None,
        };
        let fitted = fit_compressed(chain.clone(), 8);
        assert_eq!((fitted.width, fitted.height, fitted.levels.len()), (8, 8, 4));
        assert_eq!(fitted.levels[0].len(), 64);
        // Never below one block at level 0.
        let fitted = fit_compressed(chain.clone(), 1);
        assert_eq!((fitted.width, fitted.levels.len()), (4, 3));
        assert_eq!(fit_compressed(chain, 0).width, 32);
    }

    #[test]
    fn names_follow_the_pixels() {
        let material = crate::LabMaterial {
            albedo: Some(crate::LabTexture {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            }),
            ..crate::LabMaterial::default()
        };
        assert_eq!(material_name(&material), None);
        name_material_textures(&material, "M_Hull");
        assert_eq!(material_name(&material).as_deref(), Some("M_Hull"));
        let cloned = material.clone();
        assert_eq!(material_name(&cloned).as_deref(), Some("M_Hull"));
        assert_eq!(material_name(&crate::LabMaterial::default()), None);
    }
}
