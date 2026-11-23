// SPDX-License-Identifier: MPL-2.0
//! GPU memory accounting: every allocation the renderer owns, by category.
//!
//! [`memory_report`] reads the backend allocator's live allocation list
//! ([`wgpu::Device::generate_allocator_report`], Vulkan and D3D12) and sorts each allocation
//! into a [`MemoryCategory`] by its wgpu label. Labels are the contract: a texture or buffer
//! created without one lands in [`MemoryCategory::Unlabeled`], and the report lists the
//! largest such allocations so they can be named. Backends without an allocator report (GL,
//! Metal, WebGPU) return a report with `available == false`.
//!
//! [`process_rss_bytes`] adds the process's resident set (Linux and Android `/proc`, the Windows
//! working set), the number the Steam Deck budget is set against.

use std::collections::HashMap;
use std::fmt;

pub use regia_core::memory::Subsystem;

/// What an allocation is for. Matches the budget table in
/// `docs/evidence/perf/memory-20261007.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MemoryCategory {
    /// Terrain heightfield, its min/max pyramid and the sim/collision heights.
    TerrainHeight,
    /// Terrain layer texture arrays (albedo, normal, height, ORM).
    TerrainLayers,
    /// Terrain macro maps (the 8K albedo / normal tiles and the coarse fallback).
    TerrainMacro,
    /// Terrain splat weights, masks, holes, deformation and contact maps.
    TerrainMasks,
    /// Terrain mesh, instance and draw buffers (CDLOD patches, node lists).
    TerrainGeometry,
    /// Scatter instances, rock meshes and their contact map.
    Scatter,
    /// Mesh vertex/index/object data and material textures (set pieces, hero buildings).
    Meshes,
    /// Shadow maps: cascades, the local-light atlas, paged shadows.
    Shadows,
    /// Screen-sized render targets and history (HDR, depth, velocity, TAA, SSAO, SSR, bloom).
    RenderTargets,
    /// Image-based lighting, reflection probes, lightmaps, atmosphere / colour LUTs, cloud noise.
    ProbesLuts,
    /// Uniforms, per-frame data, staging, readback and query buffers.
    FrameData,
    /// Everything with a label no rule matches, and the UI glyph and VFX atlases.
    Other,
    /// Allocations created without a label.
    Unlabeled,
}

impl MemoryCategory {
    /// Every category, in report order.
    pub const ALL: [Self; 13] = [
        Self::TerrainHeight,
        Self::TerrainLayers,
        Self::TerrainMacro,
        Self::TerrainMasks,
        Self::TerrainGeometry,
        Self::Scatter,
        Self::Meshes,
        Self::Shadows,
        Self::RenderTargets,
        Self::ProbesLuts,
        Self::FrameData,
        Self::Other,
        Self::Unlabeled,
    ];

    /// Stable snake-case name (agent protocol, CSV).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::TerrainHeight => "terrain_height",
            Self::TerrainLayers => "terrain_layers",
            Self::TerrainMacro => "terrain_macro",
            Self::TerrainMasks => "terrain_masks",
            Self::TerrainGeometry => "terrain_geometry",
            Self::Scatter => "scatter",
            Self::Meshes => "meshes",
            Self::Shadows => "shadows",
            Self::RenderTargets => "render_targets",
            Self::ProbesLuts => "probes_luts",
            Self::FrameData => "frame_data",
            Self::Other => "other",
            Self::Unlabeled => "unlabeled",
        }
    }
}

impl fmt::Display for MemoryCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Ordered label rules: the first rule whose every needle is in the lower-cased label wins.
/// Terrain rules come first so "terrain shadow ..." is terrain, not shadows, only where the
/// allocation really is terrain data.
const RULES: &[(&[&str], MemoryCategory)] = &[
    (&["(wgpu internal)"], MemoryCategory::FrameData),
    (&["froxel"], MemoryCategory::RenderTargets),
    (&["cluster"], MemoryCategory::FrameData),
    (&["contact shadow"], MemoryCategory::Shadows),
    (&["contact"], MemoryCategory::TerrainMasks),
    (&["scatter"], MemoryCategory::Scatter),
    (&["rock"], MemoryCategory::Scatter),
    (&["macro"], MemoryCategory::TerrainMacro),
    (&["terrain", "layer"], MemoryCategory::TerrainLayers),
    (&["terrain", "albedo"], MemoryCategory::TerrainLayers),
    (&["terrain", "normal"], MemoryCategory::TerrainLayers),
    (&["terrain", "orm"], MemoryCategory::TerrainLayers),
    (&["terrain", "weight"], MemoryCategory::TerrainMasks),
    (&["terrain", "splat"], MemoryCategory::TerrainMasks),
    (&["terrain", "mask"], MemoryCategory::TerrainMasks),
    (&["terrain", "hole"], MemoryCategory::TerrainMasks),
    (&["terrain", "deform"], MemoryCategory::TerrainMasks),
    (&["terrain", "rut"], MemoryCategory::TerrainMasks),
    (&["terrain", "height"], MemoryCategory::TerrainHeight),
    (&["terrain", "pyramid"], MemoryCategory::TerrainHeight),
    (&["terrain", "minmax"], MemoryCategory::TerrainHeight),
    (&["terrain", "sdf"], MemoryCategory::TerrainHeight),
    (&["terrain", "shadow"], MemoryCategory::Shadows),
    (&["terrain", "sky"], MemoryCategory::ProbesLuts),
    (&["terrain", "readback"], MemoryCategory::FrameData),
    (&["terrain", "offscreen"], MemoryCategory::RenderTargets),
    (&["terrain"], MemoryCategory::TerrainGeometry),
    (&["cdlod"], MemoryCategory::TerrainGeometry),
    (&["shadow"], MemoryCategory::Shadows),
    (&["cascade"], MemoryCategory::Shadows),
    (&["csm"], MemoryCategory::Shadows),
    // Atlases that hold no shadow maps (ADR-2952): the UI's glyphs and the VFX sprites and decals
    // have no category of their own; a lightmap is baked lighting.
    (&["glyph", "atlas"], MemoryCategory::Other),
    (&["vfx", "atlas"], MemoryCategory::Other),
    (&["lightmap", "atlas"], MemoryCategory::ProbesLuts),
    (&["atlas"], MemoryCategory::Shadows),
    (&["page"], MemoryCategory::Shadows),
    (&["ibl"], MemoryCategory::ProbesLuts),
    (&["probe"], MemoryCategory::ProbesLuts),
    (&["lut"], MemoryCategory::ProbesLuts),
    (&["transmittance"], MemoryCategory::ProbesLuts),
    (&["multi-scatter"], MemoryCategory::ProbesLuts),
    (&["sky view"], MemoryCategory::ProbesLuts),
    (&["irradiance"], MemoryCategory::ProbesLuts),
    (&["noise"], MemoryCategory::ProbesLuts),
    (&["brdf"], MemoryCategory::ProbesLuts),
    (&["material texture"], MemoryCategory::Meshes),
    (&["material"], MemoryCategory::Meshes),
    (&["mesh"], MemoryCategory::Meshes),
    (&["vertex"], MemoryCategory::Meshes),
    (&["vertices"], MemoryCategory::Meshes),
    (&["index"], MemoryCategory::Meshes),
    (&["indices"], MemoryCategory::Meshes),
    (&["skin"], MemoryCategory::Meshes),
    (&["joint"], MemoryCategory::Meshes),
    (&["uniform"], MemoryCategory::FrameData),
    (&["params"], MemoryCategory::FrameData),
    (&["staging"], MemoryCategory::FrameData),
    (&["readback"], MemoryCategory::FrameData),
    (&["timestamp"], MemoryCategory::FrameData),
    (&["query"], MemoryCategory::FrameData),
    (&["upload"], MemoryCategory::FrameData),
    (&["indirect"], MemoryCategory::FrameData),
    (&["lights"], MemoryCategory::FrameData),
    (&["cluster"], MemoryCategory::FrameData),
    (&["object"], MemoryCategory::FrameData),
    (&["instance"], MemoryCategory::FrameData),
    (&["hdr"], MemoryCategory::RenderTargets),
    (&["ldr"], MemoryCategory::RenderTargets),
    (&["depth"], MemoryCategory::RenderTargets),
    (&["velocity"], MemoryCategory::RenderTargets),
    (&["history"], MemoryCategory::RenderTargets),
    (&["taa"], MemoryCategory::RenderTargets),
    (&["ssao"], MemoryCategory::RenderTargets),
    (&["ssr"], MemoryCategory::RenderTargets),
    (&["ssgi"], MemoryCategory::RenderTargets),
    (&["bloom"], MemoryCategory::RenderTargets),
    (&["hiz"], MemoryCategory::RenderTargets),
    (&["fog"], MemoryCategory::RenderTargets),
    (&["froxel"], MemoryCategory::RenderTargets),
    (&["cloud"], MemoryCategory::RenderTargets),
    (&["exposure"], MemoryCategory::RenderTargets),
    (&["exposed"], MemoryCategory::RenderTargets),
    (&["fxaa"], MemoryCategory::RenderTargets),
    (&["tonemap"], MemoryCategory::RenderTargets),
    (&["color"], MemoryCategory::RenderTargets),
    (&["target"], MemoryCategory::RenderTargets),
    (&["msaa"], MemoryCategory::RenderTargets),
    (&["post"], MemoryCategory::RenderTargets),
    (&["frame"], MemoryCategory::RenderTargets),
];

/// The category of an allocation labelled `label`.
#[must_use]
pub fn classify(label: &str) -> MemoryCategory {
    if label.is_empty() || label.starts_with("Unlabeled") || label.starts_with("<Unnamed") {
        return MemoryCategory::Unlabeled;
    }
    let lower = label.to_ascii_lowercase();
    RULES
        .iter()
        .find(|(needles, _)| needles.iter().all(|n| lower.contains(n)))
        .map_or(MemoryCategory::Other, |(_, category)| *category)
}

/// One category's total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CategoryTotal {
    /// Bytes allocated.
    pub bytes: u64,
    /// Live allocations.
    pub count: u32,
}

/// One label's total (all allocations sharing it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelTotal {
    /// The wgpu label.
    pub label: String,
    /// Its category.
    pub category: MemoryCategory,
    /// Bytes allocated.
    pub bytes: u64,
    /// Live allocations.
    pub count: u32,
}

/// The renderer's GPU memory, by category.
#[derive(Clone, Debug, Default)]
pub struct MemoryReport {
    /// `false` when the backend has no allocator report (GL, Metal, WebGPU): every total is 0.
    pub available: bool,
    /// Backend name (`Vulkan`, `Dx12`, ...).
    pub backend: String,
    /// Sum of live allocations, bytes.
    pub allocated_bytes: u64,
    /// Device memory blocks reserved by the allocator (allocated + free space in blocks), bytes.
    pub reserved_bytes: u64,
    /// Per category, in [`MemoryCategory::ALL`] order.
    pub categories: Vec<(MemoryCategory, CategoryTotal)>,
    /// Every label, largest first.
    pub labels: Vec<LabelTotal>,
    /// Process resident set at report time, bytes. Linux and Android report `VmRSS`; Windows
    /// reports the working set; macOS the task's resident size. `None` on other targets and when the
    /// OS query fails.
    pub rss_bytes: Option<u64>,
    /// The whole device's allocated bytes as the backend counts them (ADR-5860): the allocator's
    /// live total ([`Self::allocated_bytes`]) on Vulkan and D3D12, `MTLDevice.currentAllocatedSize`
    /// on Metal (driver-internal resources included, no split by label, so `available` stays
    /// `false` there). `None` on GL and WebGPU. Compare this one with a GPU memory cap on every
    /// backend.
    pub device_allocated_bytes: Option<u64>,
    /// The engine's CPU copies of mesh and texture data, process-wide, and what the host memory
    /// budget did ([`crate::host_memory::host_memory_counters`], ADR-7891). RAM, not GPU memory.
    pub host: crate::host_memory::HostMemoryCounters,
}

impl MemoryReport {
    /// Bytes in `category`.
    #[must_use]
    pub fn bytes(&self, category: MemoryCategory) -> u64 {
        self.categories
            .iter()
            .find(|(c, _)| *c == category)
            .map_or(0, |(_, t)| t.bytes)
    }

    /// Builds a report from `(label, bytes)` allocations (the testable core of [`memory_report`]).
    #[must_use]
    pub fn from_allocations<'a>(
        backend: &str,
        allocations: impl IntoIterator<Item = (&'a str, u64)>,
        reserved_bytes: u64,
    ) -> Self {
        let mut by_label: HashMap<&str, (u64, u32)> = HashMap::new();
        for (label, bytes) in allocations {
            let entry = by_label.entry(label).or_default();
            entry.0 += bytes;
            entry.1 += 1;
        }
        let mut totals: HashMap<MemoryCategory, CategoryTotal> = HashMap::new();
        let mut labels: Vec<LabelTotal> = by_label
            .into_iter()
            .map(|(label, (bytes, count))| {
                let category = classify(label);
                let total = totals.entry(category).or_default();
                total.bytes += bytes;
                total.count += count;
                LabelTotal {
                    label: label.to_owned(),
                    category,
                    bytes,
                    count,
                }
            })
            .collect();
        labels.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.label.cmp(&b.label)));
        let categories = MemoryCategory::ALL
            .iter()
            .map(|c| (*c, totals.get(c).copied().unwrap_or_default()))
            .collect();
        Self {
            available: true,
            backend: backend.to_owned(),
            allocated_bytes: labels.iter().map(|l| l.bytes).sum(),
            reserved_bytes,
            categories,
            labels,
            rss_bytes: None,
            device_allocated_bytes: None,
            host: crate::host_memory::HostMemoryCounters::default(),
        }
    }

    /// A fixed-width table: one row per non-empty category, then the `top` largest labels.
    #[must_use]
    pub fn table(&self, top: usize) -> String {
        use std::fmt::Write as _;
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        let mut out = String::new();
        if !self.available {
            let _ = writeln!(out, "memory report: not available on {}", self.backend);
            return out;
        }
        let _ = writeln!(
            out,
            "GPU memory ({}): {:.1} MiB allocated, {:.1} MiB reserved{}",
            self.backend,
            mib(self.allocated_bytes),
            mib(self.reserved_bytes),
            self.rss_bytes
                .map(|r| format!(", RSS {:.1} MiB", mib(r)))
                .unwrap_or_default()
        );
        for (category, total) in &self.categories {
            if total.count > 0 {
                let _ = writeln!(
                    out,
                    "  {:<18} {:>9.1} MiB  {:>5} allocs",
                    category.name(),
                    mib(total.bytes),
                    total.count
                );
            }
        }
        let host = &self.host;
        if host.cpu_mesh_bytes + host.cpu_texture_bytes + host.dropped_bytes > 0 {
            let _ = writeln!(
                out,
                "  host copies (RAM): mesh {:.1} MiB, texture {:.1} MiB; dropped {:.1} MiB ({}), \
                 re-read {:.1} MiB ({}), over-budget ticks {}",
                mib(host.cpu_mesh_bytes),
                mib(host.cpu_texture_bytes),
                mib(host.dropped_bytes),
                host.drops,
                mib(host.reread_bytes),
                host.rereads,
                host.over_budget_ticks
            );
        }
        for label in self.labels.iter().take(top) {
            let _ = writeln!(
                out,
                "    {:>9.2} MiB  {:>4}x  {:<16} {}",
                mib(label.bytes),
                label.count,
                label.category.name(),
                label.label
            );
        }
        out
    }
}

/// Every live GPU allocation of `device`, by category, plus the process RSS.
///
/// Cost: one pass over the allocator's block list under its lock (sub-millisecond for a few
/// thousand allocations); call it from tools and the agent protocol, not every frame.
#[must_use]
pub fn memory_report(device: &crate::RenderDevice) -> MemoryReport {
    let _span = tracing::info_span!("memory_report").entered();
    let backend = format!("{:?}", device.adapter.get_info().backend);
    let mut report = match device.device.generate_allocator_report() {
        Some(raw) => MemoryReport::from_allocations(
            &backend,
            raw.allocations.iter().map(|a| (a.name.as_str(), a.size)),
            raw.total_reserved_bytes,
        ),
        None => MemoryReport {
            backend,
            ..MemoryReport::default()
        },
    };
    report.device_allocated_bytes = if report.available {
        Some(report.allocated_bytes)
    } else {
        crate::platform::device_allocated_bytes(&device.device)
    };
    report.rss_bytes = process_rss_bytes();
    report.host = crate::host_memory::host_memory_counters();
    report
}

/// The process's resident set size in bytes: `VmRSS` on Linux and Android, the working set on
/// Windows, the task's resident size on macOS (`regia_platform::process_memory`); `None` elsewhere.
#[must_use]
pub fn process_rss_bytes() -> Option<u64> {
    regia_platform::process_memory().map(|m| m.resident_bytes)
}

/// The process's peak resident set size in bytes: `VmHWM` on Linux and Android, the peak working
/// set on Windows, `ru_maxrss` on macOS; `None` elsewhere.
#[must_use]
pub fn process_peak_rss_bytes() -> Option<u64> {
    regia_platform::process_memory().map(|m| m.peak_resident_bytes)
}

/// The coarse GPU memory buckets the 10 s telemetry summary (`memory.categories`) and the
/// device-lost crash bundle carry (ADR-223, v0.1.16): what filled the GPU, at a glance. Every
/// allocation lands in exactly one ([`summary_bucket`]); `pipeline_cache` is not a GPU
/// allocation and is reported beside them ([`pipeline_cache_bytes`]).
pub const SUMMARY_BUCKETS: [&str; 9] = [
    "meshes",
    "textures",
    "virtual_texture",
    "terrain",
    "staging",
    "atlases",
    "shadows",
    "render_targets",
    "other",
];

/// The [`SUMMARY_BUCKETS`] index of an allocation labelled `label`: material textures, the
/// terrain virtual-texture atlas, staging (and wgpu's internal buffers), atlases (shadow, VFX,
/// glyphs), then the [`MemoryCategory`] of the label (mesh and scatter data are meshes, the
/// terrain categories terrain, probes, LUTs, frame data and unlabelled allocations other).
#[must_use]
pub fn summary_bucket(label: &str) -> usize {
    let lower = label.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);
    if has("texture") && (has("material") || has("image")) {
        return 1;
    }
    if has("terrain vt") {
        return 2;
    }
    if has("(wgpu internal)") || has("staging") || has("belt") || has("upload") {
        return 4;
    }
    if has("atlas") {
        return 5;
    }
    match classify(label) {
        MemoryCategory::Meshes | MemoryCategory::Scatter => 0,
        MemoryCategory::TerrainHeight
        | MemoryCategory::TerrainLayers
        | MemoryCategory::TerrainMacro
        | MemoryCategory::TerrainMasks
        | MemoryCategory::TerrainGeometry => 3,
        MemoryCategory::Shadows => 6,
        MemoryCategory::RenderTargets => 7,
        MemoryCategory::ProbesLuts
        | MemoryCategory::FrameData
        | MemoryCategory::Other
        | MemoryCategory::Unlabeled => 8,
    }
}

/// Bytes per [`SUMMARY_BUCKETS`] entry of `allocations` (`(label, bytes)`).
#[must_use]
pub fn summary_buckets_of<'a>(allocations: impl IntoIterator<Item = (&'a str, u64)>) -> [u64; 9] {
    let mut out = [0u64; 9];
    for (label, bytes) in allocations {
        out[summary_bucket(label)] += bytes;
    }
    out
}

static LAST_BUCKETS: std::sync::Mutex<Option<[u64; 9]>> = std::sync::Mutex::new(None);

/// Publishes the bucket totals of `allocations` as the latest ([`last_summary_buckets`]), all
/// nine from one sample. The telemetry probe calls it with each allocator report it samples
/// (off the frame thread).
pub fn publish_summary_buckets<'a>(allocations: impl IntoIterator<Item = (&'a str, u64)>) {
    let totals = summary_buckets_of(allocations);
    *LAST_BUCKETS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(totals);
}

/// The bucket totals last published ([`publish_summary_buckets`]), `(name, bytes)` in
/// [`SUMMARY_BUCKETS`] order; `None` before the first (telemetry off, or a backend without an
/// allocator report).
#[must_use]
pub fn last_summary_buckets() -> Option<[(&'static str, u64); 9]> {
    let totals = (*LAST_BUCKETS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))?;
    Some(std::array::from_fn(|i| (SUMMARY_BUCKETS[i], totals[i])))
}

/// Bytes of the installed persistent pipeline cache: the larger of what was loaded (or seeded)
/// and what was last saved (`0` without a cache). RAM, not GPU memory.
#[must_use]
pub fn pipeline_cache_bytes() -> u64 {
    use crate::pipeline_cache::CacheLoad;
    let status = crate::pipeline_cache::status();
    let loaded = match status.load {
        Some(CacheLoad::Loaded(n) | CacheLoad::Seeded(n)) => n,
        _ => 0,
    };
    loaded.max(status.saved_bytes)
}

/// One line for logs and crash bundles: the last published buckets in MiB (non-empty ones),
/// the pipeline cache, and the memory-pressure state ([`crate::gpu_health`]).
#[must_use]
pub fn memory_summary_line() -> String {
    let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
    let buckets = last_summary_buckets().map_or_else(
        || "no allocator report yet".to_owned(),
        |buckets| {
            buckets
                .iter()
                .filter(|(_, bytes)| *bytes > 0)
                .map(|(name, bytes)| format!("{name} {:.1}", mib(*bytes)))
                .collect::<Vec<_>>()
                .join(", ")
        },
    );
    format!(
        "GPU memory (MiB, last probe): {buckets}; pipeline cache {:.1} MiB (RAM); pressure signals {}, \
         texture budget steps {}, out-of-memory errors {} uncaught / {} caught, mips held {}",
        mib(pipeline_cache_bytes()),
        crate::gpu_health::memory_pressure_count(),
        crate::gpu_health::texture_budget_steps(),
        crate::gpu_health::gpu_oom_count(),
        crate::gpu_health::captured_oom_count(),
        crate::gpu_health::texture_mips_held(),
    )
}

/// The engine subsystem a GPU allocation labelled `label` belongs to (ADR-539): terrain and
/// scatter data are [`Subsystem::Terrain`], VFX and particle buffers [`Subsystem::Vfx`], UI and
/// glyph textures [`Subsystem::Ui`], everything else the renderer's ([`Subsystem::Render`]).
/// The same subsystems as the CPU heap ledger (`regia_core::memory`), so one report can put
/// both side by side.
#[must_use]
pub fn gpu_subsystem(label: &str) -> Subsystem {
    let lower = label.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);
    if has("vfx") || has("particle") {
        return Subsystem::Vfx;
    }
    if has("egui") || has("glyph") {
        return Subsystem::Ui;
    }
    match classify(label) {
        MemoryCategory::TerrainHeight
        | MemoryCategory::TerrainLayers
        | MemoryCategory::TerrainMacro
        | MemoryCategory::TerrainMasks
        | MemoryCategory::TerrainGeometry
        | MemoryCategory::Scatter => Subsystem::Terrain,
        _ => Subsystem::Render,
    }
}

/// Bytes per [`Subsystem`] (indexed by its discriminant) of `allocations` (`(label, bytes)`).
#[must_use]
pub fn subsystem_bytes_of<'a>(
    allocations: impl IntoIterator<Item = (&'a str, u64)>,
) -> [u64; Subsystem::COUNT] {
    let mut out = [0u64; Subsystem::COUNT];
    for (label, bytes) in allocations {
        out[gpu_subsystem(label) as usize] += bytes;
    }
    out
}

static LAST_SUBSYSTEMS: [std::sync::atomic::AtomicU64; Subsystem::COUNT] =
    [const { std::sync::atomic::AtomicU64::new(0) }; Subsystem::COUNT];
static SUBSYSTEMS_PUBLISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Publishes the per-subsystem GPU totals of `allocations` as the latest
/// ([`last_subsystem_bytes`]). The telemetry probe calls it with each allocator report it samples.
pub fn publish_subsystem_bytes<'a>(allocations: impl IntoIterator<Item = (&'a str, u64)>) {
    use std::sync::atomic::Ordering;
    for (slot, bytes) in LAST_SUBSYSTEMS.iter().zip(subsystem_bytes_of(allocations)) {
        slot.store(bytes, Ordering::Relaxed);
    }
    SUBSYSTEMS_PUBLISHED.store(true, Ordering::Release);
}

/// The per-subsystem GPU totals last published ([`publish_subsystem_bytes`]), `(subsystem,
/// bytes)` in [`Subsystem::ALL`] order; `None` before the first (telemetry off, or a backend
/// without an allocator report).
#[must_use]
pub fn last_subsystem_bytes() -> Option<[(Subsystem, u64); Subsystem::COUNT]> {
    use std::sync::atomic::Ordering;
    if !SUBSYSTEMS_PUBLISHED.load(Ordering::Acquire) {
        return None;
    }
    Some(std::array::from_fn(|i| {
        (Subsystem::ALL[i], LAST_SUBSYSTEMS[i].load(Ordering::Relaxed))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_buckets_split_textures_vt_staging_and_atlases_out() {
        let at = |label: &str| SUMMARY_BUCKETS[summary_bucket(label)];
        assert_eq!(at("material texture (streamed)"), "textures");
        assert_eq!(at("material texture (bcn)"), "textures");
        assert_eq!(at("dynamic material texture"), "textures");
        assert_eq!(at("terrain vt atlas"), "virtual_texture");
        assert_eq!(at("staging belt"), "staging");
        assert_eq!(at("(wgpu internal) Staging"), "staging");
        assert_eq!(at("shadow atlas"), "atlases");
        assert_eq!(at("vfx atlas"), "atlases");
        assert_eq!(at("csm cascade"), "shadows");
        assert_eq!(at("mesh arena vertices"), "meshes");
        assert_eq!(at("terrain heights"), "terrain");
        assert_eq!(at("hdr target"), "render_targets");
        assert_eq!(at(""), "other");
        let totals = summary_buckets_of([
            ("material texture", 10),
            ("mesh index", 5),
            ("shadow atlas", 7),
            ("x", 1),
        ]);
        assert_eq!(totals, [5, 10, 0, 0, 0, 7, 0, 0, 1]);
        assert_eq!(totals.iter().sum::<u64>(), 23, "every byte in exactly one bucket");
    }

    #[test]
    fn published_buckets_come_from_one_sample() {
        publish_summary_buckets([("mesh index", 5), ("shadow atlas", 7)]);
        let got = last_summary_buckets().expect("published");
        let bytes: Vec<u64> = got.iter().map(|(_, b)| *b).collect();
        assert_eq!(
            bytes,
            summary_buckets_of([("mesh index", 5), ("shadow atlas", 7)])
        );
    }

    /// The Deck pass of v0.2.15 (ADR-2952) saw the 10.5 MB UI glyph atlas reported as shadows:
    /// only an atlas of shadow maps is a shadow.
    #[test]
    fn atlases_of_glyphs_vfx_and_lightmaps_are_not_shadows() {
        assert_eq!(classify("ui glyph atlas"), MemoryCategory::Other);
        assert_eq!(classify("vfx atlas"), MemoryCategory::Other);
        assert_eq!(classify("vfx decal atlas"), MemoryCategory::Other);
        assert_eq!(classify("lightmap atlas"), MemoryCategory::ProbesLuts);
        assert_eq!(classify("terrain vt atlas"), MemoryCategory::TerrainGeometry);
        assert_eq!(classify("local shadow atlas"), MemoryCategory::Shadows);
        assert_eq!(classify("atlas"), MemoryCategory::Shadows);
    }

    #[test]
    fn classifies_known_labels() {
        assert_eq!(classify("terrain heights"), MemoryCategory::TerrainHeight);
        assert_eq!(
            classify("terrain macro albedo tiles"),
            MemoryCategory::TerrainMacro
        );
        assert_eq!(classify("terrain layer albedo"), MemoryCategory::TerrainLayers);
        assert_eq!(classify("scatter contact map"), MemoryCategory::TerrainMasks);
        assert_eq!(classify("contact shadows"), MemoryCategory::Shadows);
        assert_eq!(classify("contact shadow params"), MemoryCategory::Shadows);
        assert_eq!(classify("terrain sun shadow"), MemoryCategory::Shadows);
        assert_eq!(classify("material texture"), MemoryCategory::Meshes);
        assert_eq!(classify("frame hdr"), MemoryCategory::RenderTargets);
        assert_eq!(classify("ibl prefilter"), MemoryCategory::ProbesLuts);
        assert_eq!(classify("staging belt"), MemoryCategory::FrameData);
        assert_eq!(classify("Unlabeled texture"), MemoryCategory::Unlabeled);
        assert_eq!(classify("froxel scatter"), MemoryCategory::RenderTargets);
        assert_eq!(classify("cluster indices"), MemoryCategory::FrameData);
        assert_eq!(classify("(wgpu internal) Staging"), MemoryCategory::FrameData);
        assert_eq!(classify("something new"), MemoryCategory::Other);
    }

    #[test]
    fn totals_by_category_and_label() {
        let report = MemoryReport::from_allocations(
            "Vulkan",
            [
                ("frame hdr", 100),
                ("frame hdr", 50),
                ("terrain heights", 400),
                ("Unlabeled buffer", 7),
            ],
            1024,
        );
        assert!(report.available);
        assert_eq!(report.allocated_bytes, 557);
        assert_eq!(report.reserved_bytes, 1024);
        assert_eq!(report.bytes(MemoryCategory::RenderTargets), 150);
        assert_eq!(report.bytes(MemoryCategory::TerrainHeight), 400);
        assert_eq!(report.bytes(MemoryCategory::Unlabeled), 7);
        assert_eq!(report.labels[0].label, "terrain heights");
        assert_eq!(report.labels[1].count, 2);
        let sum: u64 = report.categories.iter().map(|(_, t)| t.bytes).sum();
        assert_eq!(sum, report.allocated_bytes);
        assert!(report.table(3).contains("terrain_height"));
    }

    #[test]
    fn gpu_allocations_map_to_subsystems() {
        assert_eq!(gpu_subsystem("terrain heights"), Subsystem::Terrain);
        assert_eq!(gpu_subsystem("scatter instances"), Subsystem::Terrain);
        assert_eq!(gpu_subsystem("rock mesh vertices"), Subsystem::Terrain);
        assert_eq!(gpu_subsystem("vfx particle state"), Subsystem::Vfx);
        assert_eq!(gpu_subsystem("egui texture"), Subsystem::Ui);
        assert_eq!(gpu_subsystem("frame hdr"), Subsystem::Render);
        assert_eq!(gpu_subsystem(""), Subsystem::Render);
        let totals = subsystem_bytes_of([
            ("terrain heights", 400),
            ("frame hdr", 100),
            ("vfx atlas", 7),
            ("material texture", 30),
        ]);
        assert_eq!(totals[Subsystem::Terrain as usize], 400);
        assert_eq!(totals[Subsystem::Render as usize], 130);
        assert_eq!(totals[Subsystem::Vfx as usize], 7);
        assert_eq!(
            totals.iter().sum::<u64>(),
            537,
            "every byte in exactly one subsystem"
        );
        publish_subsystem_bytes([("terrain heights", 400), ("frame hdr", 100)]);
        let last = last_subsystem_bytes().expect("published");
        assert_eq!(last[Subsystem::Terrain as usize], (Subsystem::Terrain, 400));
    }

    #[cfg(any(target_os = "linux", target_os = "android", windows))]
    #[test]
    fn rss_is_readable() {
        assert!(process_rss_bytes().is_some_and(|b| b > 0));
        // The peak, read after the current value, is never below it (Windows: peak working set,
        // ADR-3288).
        let now = process_rss_bytes();
        let peak = process_peak_rss_bytes();
        assert!(
            peak.is_some_and(|p| now.is_some_and(|n| p >= n)),
            "{now:?} {peak:?}"
        );
    }
}
