// SPDX-License-Identifier: MPL-2.0
//! Bindless materials, step 3 (ADR-863, after ADR-316 and ADR-356): one per-device texture
//! table that every standard material samples through, instead of seven texture bindings in
//! each material's own bind group.
//!
//! - **The gate.** A device draws through the table only when it asked for it and can carry it
//!   ([`table_support`]): the bindless level (`REGIA_BINDLESS_LEVEL`, read once when the device
//!   is created, [`requested_level`]) is at least [`TEXTURE_TABLE_LEVEL`], the device has
//!   [`TABLE_FEATURES`], its tier is [`crate::BindlessTier::TextureArrays`], and it has a fifth
//!   bind group. Every other device keeps the per-material texture bindings of earlier releases,
//!   and its shaders, layouts and draws are unchanged.
//! - **Descriptor limits.** The table holds [`table_capacity`] views: the device's binding-array
//!   budget minus [`TEXTURE_TABLE_HEADROOM`] for the other textures a material pass samples
//!   (Vulkan counts every sampled image of a stage against the update-after-bind limit that
//!   bounds binding arrays), at most [`TEXTURE_TABLE_MAX`] (or what [`TABLE_VIEWS_ENV`] asks for,
//!   [`requested_views`]). A scene past it binds the slot's
//!   neutral fallback for the textures that did not fit, counts them, and warns once.
//! - **Binding.** Indices 0-3 hold the renderer's neutral views (white, flat normal, mid grey,
//!   aux), and every free index in the CPU array holds the white view. A device with
//!   `PARTIALLY_BOUND_BINDING_ARRAY` (Vulkan and DX12) binds a power-of-two prefix of at least
//!   256 views that covers every live index. Every other device, including Metal, binds the
//!   whole table.
//! - **Rebuilds.** wgpu bind groups are immutable, so a changed element means a new table group.
//!   Acquiring or releasing a texture only marks the table dirty; `TextureTable::refresh`
//!   rebuilds it at most once per frame, before the first pass binds it, and counts the
//!   rebuilds and their CPU time ([`TextureTableStats`]).
//! - **Records.** A material holds one table index per texture slot (refcounted by view
//!   identity, so materials that share a texture share its index). The indices travel in the
//!   material's arena record (`TableIndices::words`, `LabMaterial::textures` in the shaders), so
//!   a residency swap rewrites 16 bytes of the record and never rebuilds the material's group.

use std::collections::HashMap;
use std::hash::Hash;

use naga_oil::compose::ShaderDefValue;

/// The shader define that moves material texture sampling to the table (`lab_forward.wgsl`,
/// `translucent_shadow.wgsl`). Only composed with [`crate::bindless::MATERIAL_SSBO_DEF`] and never
/// for material-graph modules (graph materials keep their own texture bindings).
pub const TEXTURE_TABLE_DEF: &str = "TEXTURE_TABLE";

/// The shader define that carries the table's element count (`binding_array<…, N>`).
pub const TEXTURE_TABLE_SIZE_DEF: &str = "TEXTURE_TABLE_SIZE";

/// Bind group index of the table: groups 0-3 are the view, object, material and pass-specific
/// (glass, refraction, mask decals) groups.
pub const TEXTURE_TABLE_GROUP: u32 = 4;

/// The bindless level that turns the table on (ADR-356 numbering: 0 uniform, 1 material
/// storage arena, 2 per-draw slot, 3 texture table, 4 single-pass resolve).
pub const TEXTURE_TABLE_LEVEL: u32 = 3;

/// The highest level [`requested_level`] accepts.
pub const MAX_BINDLESS_LEVEL: u32 = 4;

/// The level of a device created without `REGIA_BINDLESS_LEVEL`: the highest level whose
/// acceptance has passed on the gate (step 1). Raised when the table's acceptance passes.
pub const DEFAULT_BINDLESS_LEVEL: u32 = 1;

/// Environment variable read once per device creation (`device.rs`): the bindless level.
pub const BINDLESS_LEVEL_ENV: &str = "REGIA_BINDLESS_LEVEL";

/// Default most views the table holds, whatever the device allows ([`TABLE_VIEWS_ENV`] overrides
/// it; the bind group rebuild walks all of them, so a larger table costs CPU on every rebuild).
pub const TEXTURE_TABLE_MAX: u32 = 4096;

/// Environment variable read once per process: the most views a device's table may hold, in
/// place of [`TEXTURE_TABLE_MAX`] ([`requested_views`]). The device's binding-array budget still
/// caps it. For scenes that report `texture_table_full`.
pub const TABLE_VIEWS_ENV: &str = "REGIA_TEXTURE_TABLE_VIEWS";

/// Largest table [`TABLE_VIEWS_ENV`] may ask for: the record packs 16-bit indices.
pub const TEXTURE_TABLE_VIEWS_LIMIT: u32 = 1 << 16;

/// Binding-array elements kept back for the other sampled textures of a material pass (the
/// frame group's shadow, light and environment textures, glass and decal groups).
pub const TEXTURE_TABLE_HEADROOM: u32 = 64;

/// Indices 0-3: the neutral views every material slot falls back to.
pub const RESERVED_INDICES: u32 = 4;

/// Texture slots of a standard material, in bind order: base colour, normal, ORM, emissive,
/// detail, detail normal, aux.
pub const MATERIAL_TEXTURE_SLOTS: usize = 7;

/// The neutral index of each slot ([`MATERIAL_TEXTURE_SLOTS`] order): white for colour, ORM and
/// emissive, flat for both normals, mid grey for detail, the aux view for aux. The same views
/// a material without the map binds on the per-material path.
pub const SLOT_FALLBACK: [u32; MATERIAL_TEXTURE_SLOTS] = [0, 1, 0, 0, 2, 1, 3];

/// Bytes the indices add to a standard material's arena record (one `vec4<u32>`).
pub const TABLE_INDEX_BYTES: u64 = 16;

/// `render.problems` code while texture slots fall back to their neutral view because the table
/// was full (`TextureTableStats::overflows` above zero).
pub const PROBLEM_TEXTURE_TABLE_FULL: &str = "texture_table_full";

/// The `render.problems` entry for a table with `stats` ([`PROBLEM_TEXTURE_TABLE_FULL`]), or
/// `None` while every texture fitted.
#[must_use]
pub fn table_problem(stats: &TextureTableStats) -> Option<crate::host_checklist::FrameProblem> {
    (stats.overflows > 0).then(|| {
        // The declared size grows only across launches (ADR-1110): name the size that fits.
        let grow = crate::table_heap::suggested_views(stats.capacity, stats.live, stats.overflows)
            .map(|views| format!(" or launch with {TABLE_VIEWS_ENV}={views} (up to the device's budget)"))
            .unwrap_or_default();
        crate::host_checklist::FrameProblem {
            code: PROBLEM_TEXTURE_TABLE_FULL,
            message: format!(
                "material texture table full ({} views, {} textures held): {} texture slots sample their \
                 neutral view instead; share or atlas textures{grow}, or run at REGIA_BINDLESS_LEVEL=1",
                stats.capacity, stats.live, stats.overflows
            ),
        }
    })
}

/// Device features the table needs: binding arrays of textures, and indexing them from a
/// material record (the index is uniform per draw, but naga may still mark it non-uniform).
pub const TABLE_FEATURES: wgpu::Features = wgpu::Features::TEXTURE_BINDING_ARRAY
    .union(wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING);

/// Why a device draws without the texture table (stable lowercase names, as the agent reports
/// them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TableFallback {
    /// The bindless level is below [`TEXTURE_TABLE_LEVEL`], so the device did not ask for the
    /// features.
    NotRequested,
    /// Material constants use per-material uniform buffers (tier `none`, or Android).
    UniformMaterials,
    /// GL or WebGPU: no binding arrays through wgpu.
    Backend,
    /// No `TEXTURE_BINDING_ARRAY`.
    NoBindingArrays,
    /// No `SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING`.
    NoNonUniformIndexing,
    /// Fewer than [`crate::bindless::TEXTURE_ARRAY_MIN_ELEMENTS`] binding-array elements per stage.
    ArrayElements,
    /// `max_bind_groups` leaves no group [`TEXTURE_TABLE_GROUP`].
    BindGroups,
}

impl TableFallback {
    /// Stable lowercase name: `not_requested`, `uniform_materials`, `backend`,
    /// `no_binding_arrays`, `no_non_uniform_indexing`, `array_elements`, `bind_groups`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRequested => "not_requested",
            Self::UniformMaterials => "uniform_materials",
            Self::Backend => "backend",
            Self::NoBindingArrays => "no_binding_arrays",
            Self::NoNonUniformIndexing => "no_non_uniform_indexing",
            Self::ArrayElements => "array_elements",
            Self::BindGroups => "bind_groups",
        }
    }

    /// Every fallback code, for catalogues and tests.
    pub const ALL: [Self; 7] = [
        Self::NotRequested,
        Self::UniformMaterials,
        Self::Backend,
        Self::NoBindingArrays,
        Self::NoNonUniformIndexing,
        Self::ArrayElements,
        Self::BindGroups,
    ];

    /// One sentence on why the device draws without the table (for `render.bindless_report`;
    /// the code, [`TableFallback::as_str`], is the stable part).
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::NotRequested => {
                "the adapter could carry the texture table, but the bindless level is below 3"
            }
            Self::UniformMaterials => {
                "material constants use per-material uniform buffers (tier none, or Android)"
            }
            Self::Backend => "this graphics backend (GL or WebGPU) has no binding arrays through wgpu",
            Self::NoBindingArrays => "the device has no texture binding arrays",
            Self::NoNonUniformIndexing => {
                "the device cannot index texture binding arrays with a per-pixel value"
            }
            Self::ArrayElements => {
                "the device allows fewer than 4096 binding-array elements per shader stage"
            }
            Self::BindGroups => "the device allows too few bind groups for the table's group 4",
        }
    }

    /// What to change to get the table, or `None` when only another device helps.
    #[must_use]
    pub fn hint(self) -> Option<&'static str> {
        match self {
            Self::NotRequested => Some("set REGIA_BINDLESS_LEVEL=3 before the device is created"),
            Self::Backend => Some("run on Vulkan, DX12 or Metal"),
            Self::UniformMaterials
            | Self::NoBindingArrays
            | Self::NoNonUniformIndexing
            | Self::ArrayElements
            | Self::BindGroups => None,
        }
    }
}

impl std::fmt::Display for TableFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a device samples material textures through the table, and how large it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableSupport {
    /// The table path, with this many views (reserved indices included).
    Table {
        /// Views in the table ([`table_capacity`]; for a device, [`table_capacity_with`] the
        /// process's [`TABLE_VIEWS_ENV`] setting).
        capacity: u32,
    },
    /// The per-material texture bindings, and why.
    Fallback(TableFallback),
}

impl TableSupport {
    /// The table's view count on the table path.
    #[must_use]
    pub fn capacity(self) -> Option<u32> {
        match self {
            Self::Table { capacity } => Some(capacity),
            Self::Fallback(_) => None,
        }
    }

    /// The reason a device stays on the per-material bindings.
    #[must_use]
    pub fn fallback(self) -> Option<TableFallback> {
        match self {
            Self::Table { .. } => None,
            Self::Fallback(reason) => Some(reason),
        }
    }
}

/// The views the table holds on a device with these `limits`: the binding-array budget less
/// [`TEXTURE_TABLE_HEADROOM`], at most [`TEXTURE_TABLE_MAX`].
#[must_use]
pub fn table_capacity(limits: &wgpu::Limits) -> u32 {
    table_capacity_with(limits, TEXTURE_TABLE_MAX)
}

/// [`table_capacity`] with `max` in place of [`TEXTURE_TABLE_MAX`].
#[must_use]
pub fn table_capacity_with(limits: &wgpu::Limits, max: u32) -> u32 {
    limits
        .max_binding_array_elements_per_shader_stage
        .saturating_sub(TEXTURE_TABLE_HEADROOM)
        .min(max)
}

/// The most views asked for by `value` (the [`TABLE_VIEWS_ENV`] text): a whole number clamped
/// to `TEXTURE_ARRAY_MIN_ELEMENTS..=`[`TEXTURE_TABLE_VIEWS_LIMIT`], or [`TEXTURE_TABLE_MAX`] when
/// unset, empty or not a number.
#[must_use]
pub fn requested_views(value: Option<&str>) -> u32 {
    value
        .map(str::trim)
        .and_then(|text| text.parse::<u32>().ok())
        .map_or(TEXTURE_TABLE_MAX, |views| {
            views.clamp(
                crate::bindless::TEXTURE_ARRAY_MIN_ELEMENTS,
                TEXTURE_TABLE_VIEWS_LIMIT,
            )
        })
}

/// [`requested_views`] of the process environment, read once: every device of the process
/// agrees on the table size its shaders and layouts are built with.
fn configured_views() -> u32 {
    static VIEWS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *VIEWS.get_or_init(|| requested_views(regia_core::brand::env_var(TABLE_VIEWS_ENV).ok().as_deref()))
}

/// Decides the table path from a device's (or adapter's) `features`, `limits` and `backend`,
/// and whether its material constants live in the storage arena (`material_ssbo`). Pure: the
/// same inputs give the same answer. The checks run in a fixed order, so the reason names the
/// first missing requirement.
#[must_use]
pub fn table_support(
    features: wgpu::Features,
    limits: &wgpu::Limits,
    backend: wgpu::Backend,
    material_ssbo: bool,
) -> TableSupport {
    let reason = if !material_ssbo {
        Some(TableFallback::UniformMaterials)
    } else if matches!(backend, wgpu::Backend::Gl | wgpu::Backend::BrowserWebGpu) {
        Some(TableFallback::Backend)
    } else if !features.contains(wgpu::Features::TEXTURE_BINDING_ARRAY) {
        Some(TableFallback::NoBindingArrays)
    } else if !features
        .contains(wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING)
    {
        Some(TableFallback::NoNonUniformIndexing)
    } else if limits.max_binding_array_elements_per_shader_stage < crate::bindless::TEXTURE_ARRAY_MIN_ELEMENTS
    {
        Some(TableFallback::ArrayElements)
    } else if limits.max_bind_groups <= TEXTURE_TABLE_GROUP {
        Some(TableFallback::BindGroups)
    } else {
        None
    };
    match reason {
        Some(reason) => TableSupport::Fallback(reason),
        None => TableSupport::Table {
            capacity: table_capacity(limits),
        },
    }
}

/// [`table_support`] for a device: its own features (the table features are present only when
/// the device asked for them, [`device_features_for_level`]), limits, backend and material path.
#[must_use]
pub fn device_support(device: &wgpu::Device) -> TableSupport {
    let limits = device.limits();
    match table_support(
        device.features(),
        &limits,
        device.adapter_info().backend,
        crate::bindless::material_ssbo(device),
    ) {
        // The process may size the table itself (`TABLE_VIEWS_ENV`).
        TableSupport::Table { .. } => TableSupport::Table {
            capacity: table_capacity_with(&limits, configured_views()),
        },
        fallback => fallback,
    }
}

/// The bindless level asked for by `value` (the `REGIA_BINDLESS_LEVEL` text): a whole number
/// 0..=[`MAX_BINDLESS_LEVEL`] (larger numbers are capped), [`DEFAULT_BINDLESS_LEVEL`] when unset,
/// empty or not a number.
#[must_use]
pub fn requested_level(value: Option<&str>) -> u32 {
    value
        .map(str::trim)
        .and_then(|text| text.parse::<u64>().ok())
        .map_or(DEFAULT_BINDLESS_LEVEL, |level| {
            u32::try_from(level.min(u64::from(MAX_BINDLESS_LEVEL))).unwrap_or(MAX_BINDLESS_LEVEL)
        })
}

/// [`requested_level`] of the process environment ([`BINDLESS_LEVEL_ENV`]). A value that is not
/// a number is logged and ignored.
#[must_use]
pub fn requested_level_from_env() -> u32 {
    let value = regia_core::brand::env_var(BINDLESS_LEVEL_ENV).ok();
    if let Some(text) = value.as_deref()
        && !text.trim().is_empty()
        && text.trim().parse::<u64>().is_err()
    {
        tracing::warn!(
            target: "regia_render",
            "{BINDLESS_LEVEL_ENV}={text:?} is not a level (0-{MAX_BINDLESS_LEVEL}); using {DEFAULT_BINDLESS_LEVEL}"
        );
    }
    requested_level(value.as_deref())
}

/// The level a render profile asked of the next device (ADR-2071); 0 when none did.
static DEVICE_LEVEL_REQUEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A render profile asks the next device for `level`: the window's device takes the higher of this
/// and [`requested_level_from_env`]. Set before the first window's device (the render plugin does
/// when [`crate::VisibilityPath::single_pass_resolve`] is on); a live device keeps its level.
pub fn set_device_level_request(level: u32) {
    DEVICE_LEVEL_REQUEST.store(
        level.min(MAX_BINDLESS_LEVEL),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// The level the next device takes: the environment's, or a higher one a profile asked for
/// ([`set_device_level_request`]).
#[must_use]
pub fn requested_level_now() -> u32 {
    requested_level_from_env().max(DEVICE_LEVEL_REQUEST.load(std::sync::atomic::Ordering::Relaxed))
}

/// The features a device created at `level` requests for the table: [`TABLE_FEATURES`] when the
/// level asks for the table and the adapter has both, else none (the device then reports
/// [`TableFallback::NotRequested`] or the missing feature).
#[must_use]
pub fn device_features_for_level(level: u32, adapter_features: wgpu::Features) -> wgpu::Features {
    if level >= TEXTURE_TABLE_LEVEL && adapter_features.contains(TABLE_FEATURES) {
        TABLE_FEATURES
    } else {
        wgpu::Features::empty()
    }
}

/// [`device_support`], with [`TableFallback::NotRequested`] for a device whose adapter could
/// carry the table but which was created below [`TEXTURE_TABLE_LEVEL`].
#[must_use]
pub fn device_support_with_adapter(device: &wgpu::Device, adapter_features: wgpu::Features) -> TableSupport {
    let support = device_support(device);
    let missing_on_device = matches!(
        support,
        TableSupport::Fallback(TableFallback::NoBindingArrays | TableFallback::NoNonUniformIndexing)
    );
    if missing_on_device && adapter_features.contains(TABLE_FEATURES) {
        TableSupport::Fallback(TableFallback::NotRequested)
    } else {
        support
    }
}

/// Adds [`TEXTURE_TABLE_DEF`] and [`TEXTURE_TABLE_SIZE_DEF`] to `defs` when `device` draws
/// standard materials through the table. Material-graph modules never call this.
pub(crate) fn add_table_defs(device: &wgpu::Device, defs: &mut HashMap<String, ShaderDefValue>) {
    if let Some(capacity) = device_support(device).capacity() {
        defs.insert(TEXTURE_TABLE_DEF.to_string(), ShaderDefValue::Bool(true));
        defs.insert(TEXTURE_TABLE_SIZE_DEF.to_string(), ShaderDefValue::UInt(capacity));
        // Residency feedback (ADR-1110): the table group's feedback words, when the process
        // asked for them and the device has room.
        if crate::residency_feedback::device_feedback_mode(device)
            != crate::residency_feedback::FeedbackMode::Off
        {
            defs.insert(
                crate::residency_feedback::RESIDENCY_FEEDBACK_DEF.to_string(),
                ShaderDefValue::Bool(true),
            );
        }
    }
}

/// The table's bind group layout on `device` (`None` off the table path): binding 0 the
/// `binding_array<texture_2d<f32>, capacity>`, binding 1 the material sampler. Layouts with equal
/// entries are one layout on a device, so every pipeline site can ask for it.
#[must_use]
pub(crate) fn table_layout(device: &wgpu::Device) -> Option<wgpu::BindGroupLayout> {
    let capacity = device_support(device).capacity()?;
    let mut entries = table_layout_entries(capacity).to_vec();
    // Residency feedback (ADR-1110): binding 2, the feedback words, only while it is on.
    if crate::residency_feedback::device_feedback_mode(device) != crate::residency_feedback::FeedbackMode::Off
    {
        entries.push(crate::residency_feedback::ResidencyFeedback::layout_entry());
    }
    Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("material texture table bgl"),
        entries: &entries,
    }))
}

/// The fixed entries of [`table_layout`]: the array, the material sampler, and the other two of
/// the shared sampler set at bindings 3 and 4 (binding 2 is the residency feedback's).
fn table_layout_entries(capacity: u32) -> [wgpu::BindGroupLayoutEntry; 4] {
    let sampler = |binding, ty| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Sampler(ty),
        count: None,
    };
    [
        wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: std::num::NonZeroU32::new(capacity.max(1)),
        },
        sampler(1, wgpu::SamplerBindingType::Filtering),
        sampler(3, wgpu::SamplerBindingType::Filtering),
        sampler(4, wgpu::SamplerBindingType::Filtering),
    ]
}

/// `prefix` (groups 0..) followed by the table layout at [`TEXTURE_TABLE_GROUP`] when `table` is
/// set (empty groups in between). Without a table it is `prefix` unchanged, so pipeline layouts
/// off the table path are the ones of earlier releases.
#[must_use]
pub(crate) fn with_table<'a>(
    prefix: &[Option<&'a wgpu::BindGroupLayout>],
    table: Option<&'a wgpu::BindGroupLayout>,
) -> Vec<Option<&'a wgpu::BindGroupLayout>> {
    let mut layouts = prefix.to_vec();
    if let Some(table) = table {
        layouts.resize(TEXTURE_TABLE_GROUP as usize, None);
        layouts.push(Some(table));
    }
    layouts
}

/// A standard material's table indices, one per texture slot ([`MATERIAL_TEXTURE_SLOTS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TableIndices(pub(crate) [u32; MATERIAL_TEXTURE_SLOTS], pub(crate) u16);

impl TableIndices {
    /// The neutral indices ([`SLOT_FALLBACK`]).
    pub(crate) const FALLBACK: Self = Self(SLOT_FALLBACK, 0);

    /// The record words the shaders read (`LabMaterial::textures`): two 16-bit indices per word,
    /// slot `2k` in the low half of word `k`. Word 3's high half holds the per-slot sampler choice
    /// ([`MaterialSamplers::bits`], ADR-2071), zero at the default.
    #[must_use]
    pub(crate) fn words(self) -> [u32; 4] {
        let i = self.0.map(|index| index & 0xffff);
        [
            i[0] | (i[1] << 16),
            i[2] | (i[3] << 16),
            i[4] | (i[5] << 16),
            i[6] | (u32::from(self.1) << 16),
        ]
    }
}

/// Which of the table's shared samplers a material texture slot reads (ADR-2071).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MaterialSampler {
    /// Linear, repeating, with the material anisotropy: the sampler of every earlier release.
    #[default]
    Repeat,
    /// Linear, clamped to the edge, with the material anisotropy.
    Clamp,
    /// Nearest, clamped to the edge.
    PointClamp,
}

impl MaterialSampler {
    /// The name `render.bindless_report` lists (`repeat`, `clamp`, `point_clamp`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repeat => "repeat",
            Self::Clamp => "clamp",
            Self::PointClamp => "point_clamp",
        }
    }

    /// Every sampler of the set, in shader choice order.
    pub const ALL: [Self; 3] = [Self::Repeat, Self::Clamp, Self::PointClamp];
}

/// A material's sampler per glTF texture slot (ADR-2071). Honored on the texture table path;
/// every other path binds one sampler per material and reads [`MaterialSampler::Repeat`], so a
/// choice other than the default draws differently off the table.
/// `render.bindless_report` reports `shared_samplers`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MaterialSamplers {
    /// Base colour.
    pub base_color: MaterialSampler,
    /// Normal map.
    pub normal: MaterialSampler,
    /// Occlusion, roughness, metallic.
    pub orm: MaterialSampler,
    /// Emissive.
    pub emissive: MaterialSampler,
}

impl MaterialSamplers {
    /// Every slot at [`MaterialSampler::Repeat`], in a constant.
    pub const DEFAULT: Self = Self {
        base_color: MaterialSampler::Repeat,
        normal: MaterialSampler::Repeat,
        orm: MaterialSampler::Repeat,
        emissive: MaterialSampler::Repeat,
    };

    /// The record's top half-word: 2 bits per slot, base colour lowest (`table_sampler_choice` in
    /// the shaders).
    #[must_use]
    pub(crate) fn bits(self) -> u16 {
        let bits = |sampler: MaterialSampler| match sampler {
            MaterialSampler::Repeat => 0u16,
            MaterialSampler::Clamp => 1,
            MaterialSampler::PointClamp => 2,
        };
        bits(self.base_color) | (bits(self.normal) << 2) | (bits(self.orm) << 4) | (bits(self.emissive) << 6)
    }
}

/// Fixed-capacity, refcounted index allocator of the table (no GPU state). Equal keys share an
/// index; an index is free again when its last holder releases it. Indices below
/// [`RESERVED_INDICES`] are never handed out.
#[derive(Debug)]
pub(crate) struct TableSlots<K> {
    capacity: u32,
    by_key: HashMap<K, (u32, u32)>,
    /// The key at each handed-out index (`index - RESERVED_INDICES`).
    keys: Vec<Option<K>>,
    /// Free indices below `next`, handed out lowest first (ADR-1110), so the live range stays a
    /// compact prefix the table group can bind (`crate::table_heap`).
    free: std::collections::BTreeSet<u32>,
    /// One past the highest index handed out and still held (the high-water mark).
    next: u32,
}

impl<K: Clone + Eq + Hash> TableSlots<K> {
    /// An empty allocator for a table of `capacity` views.
    pub(crate) fn new(capacity: u32) -> Self {
        Self {
            capacity,
            by_key: HashMap::new(),
            keys: Vec::new(),
            free: std::collections::BTreeSet::new(),
            next: RESERVED_INDICES,
        }
    }

    /// The index of `key`, shared when another holder has it. `None` when the table is full
    /// (nothing changes). The second value is `true` when the index is new to `key`, so the
    /// caller writes the view.
    pub(crate) fn acquire(&mut self, key: &K) -> Option<(u32, bool)> {
        if let Some((index, refs)) = self.by_key.get_mut(key) {
            *refs += 1;
            return Some((*index, false));
        }
        let index = match self.free.pop_first() {
            Some(index) => index,
            None if self.next < self.capacity => {
                self.next += 1;
                self.keys.push(None);
                self.next - 1
            }
            None => return None,
        };
        self.by_key.insert(key.clone(), (index, 1));
        if let Some(slot) = self.keys.get_mut((index - RESERVED_INDICES) as usize) {
            *slot = Some(key.clone());
        }
        Some((index, true))
    }

    /// Drops one hold on `index`. Returns `true` when that was the last one (the index is free
    /// and the caller resets its view). Reserved and unknown indices change nothing.
    pub(crate) fn release(&mut self, index: u32) -> bool {
        let Some(offset) = index.checked_sub(RESERVED_INDICES) else {
            return false;
        };
        let Some(Some(key)) = self.keys.get(offset as usize) else {
            return false;
        };
        let Some((_, refs)) = self.by_key.get_mut(key) else {
            return false;
        };
        *refs -= 1;
        if *refs > 0 {
            return false;
        }
        let key = key.clone();
        self.by_key.remove(&key);
        self.keys[offset as usize] = None;
        self.free.insert(index);
        // Freeing the top lowers the high-water mark past every free index under it.
        while self.next > RESERVED_INDICES && self.free.last() == Some(&(self.next - 1)) {
            self.free.pop_last();
            self.next -= 1;
            self.keys.pop();
        }
        true
    }

    /// One past the highest index held (`RESERVED_INDICES` when none is): every live index is
    /// below it.
    #[must_use]
    pub(crate) fn high_water(&self) -> u32 {
        self.next
    }

    /// Whether `key` holds an index.
    #[must_use]
    pub(crate) fn holds(&self, key: &K) -> bool {
        self.by_key.contains_key(key)
    }

    /// Moves `index` from its key to `key` in place, keeping its hold (a residency swap of a
    /// texture only one holder uses, ADR-1110). `false`, changing nothing, when `index` is free
    /// or reserved, it has more than one hold, or `key` already holds an index.
    pub(crate) fn rekey(&mut self, index: u32, key: &K) -> bool {
        let Some(offset) = index.checked_sub(RESERVED_INDICES) else {
            return false;
        };
        let Some(Some(old)) = self.keys.get(offset as usize) else {
            return false;
        };
        if self.by_key.get(old).is_none_or(|&(_, refs)| refs != 1) || self.by_key.contains_key(key) {
            return false;
        }
        let old = old.clone();
        self.by_key.remove(&old);
        self.by_key.insert(key.clone(), (index, 1));
        self.keys[offset as usize] = Some(key.clone());
        true
    }

    /// Distinct keys held now.
    #[must_use]
    pub(crate) fn live(&self) -> usize {
        self.by_key.len()
    }

    /// Holds on `index` (0 when free or reserved).
    #[cfg(test)]
    fn refs(&self, index: u32) -> u32 {
        let Some(offset) = index.checked_sub(RESERVED_INDICES) else {
            return 0;
        };
        self.keys
            .get(offset as usize)
            .and_then(Option::as_ref)
            .and_then(|key| self.by_key.get(key))
            .map_or(0, |(_, refs)| *refs)
    }
}

/// What the texture table holds and how often it was rebuilt
/// ([`crate::FrameRenderer::texture_table_stats`], `render.get_frame_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TextureTableStats {
    /// Views in the table (reserved indices included).
    pub capacity: u32,
    /// Distinct material textures holding an index.
    pub live: u64,
    /// Material texture slots that did not fit and sample their neutral view instead.
    pub overflows: u64,
    /// Table bind groups built (the first included).
    pub rebuilds: u64,
    /// Frames that refreshed the table (at most one rebuild each).
    pub refreshes: u64,
    /// CPU time of the last rebuild, microseconds (reported, never asserted).
    pub last_rebuild_us: u64,
    /// Shared material record groups built (one per material arena buffer): every standard
    /// material binds this one group at its own dynamic offset, so the count stays flat while
    /// materials come and go (ADR-863).
    pub record_groups: u64,
    /// Whether the device binds a prefix of the table (`PARTIALLY_BOUND_BINDING_ARRAY`,
    /// ADR-1110); without it every rebuild binds all [`Self::capacity`] views.
    pub partially_bound: bool,
    /// Views the last rebuild bound ([`crate::table_heap::bound_len`]).
    pub bound: u32,
    /// One past the highest index held now (every live index is below it).
    pub high_water: u32,
    /// Times the bound length changed.
    pub bound_changes: u64,
    /// Views bound over every rebuild: the rebuild cost in descriptor writes (deterministic,
    /// unlike [`Self::last_rebuild_us`]).
    pub views_written: u64,
    /// Residency swaps that replaced a texture at its own index (the record kept its indices).
    pub in_place_swaps: u64,
    /// Residency swap slots that had to move to another index (a shared texture, or the new
    /// view already in the table); the record was rewritten for them.
    pub moved_swaps: u64,
}

/// The per-device texture table: the views, the index allocator, and the bind group built from
/// them ([`TEXTURE_TABLE_GROUP`]).
pub(crate) struct TextureTable {
    slots: TableSlots<wgpu::TextureView>,
    /// The neutral views at indices 0-3.
    reserved: [wgpu::TextureView; RESERVED_INDICES as usize],
    /// One view per index; free indices hold the white view.
    views: Vec<wgpu::TextureView>,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// The rest of the shared sampler set (ADR-2071): clamp, then point clamp.
    clamp_samplers: [wgpu::Sampler; 2],
    group: Option<wgpu::BindGroup>,
    dirty: bool,
    warned_full: bool,
    stats: TextureTableStats,
    /// The bound length policy's state (`crate::table_heap`).
    heap: crate::table_heap::HeapState,
    /// The refresh count when each index was last given to a new texture (stale feedback).
    assigned: Vec<u64>,
    /// GPU residency feedback (`crate::residency_feedback`), when on.
    feedback: Option<crate::residency_feedback::ResidencyFeedback>,
}

impl TextureTable {
    /// A table of `capacity` views on `device` (its [`table_layout`]), every index on its neutral
    /// view: `reserved` is white, flat normal, mid grey and aux, in that order. `sampler` is the
    /// material sampler, and `anisotropy` its clamp, which the clamped sampler of the set shares.
    /// `None` off the table path.
    pub(crate) fn new(
        device: &wgpu::Device,
        reserved: [&wgpu::TextureView; RESERVED_INDICES as usize],
        sampler: &wgpu::Sampler,
        anisotropy: u16,
    ) -> Option<Self> {
        let capacity = device_support(device).capacity()?;
        let layout = table_layout(device)?;
        let reserved = reserved.map(Clone::clone);
        let mut views = vec![reserved[0].clone(); capacity as usize];
        for (slot, view) in views.iter_mut().zip(&reserved) {
            *slot = view.clone();
        }
        let partially_bound = device.features().contains(crate::table_heap::PARTIAL_FEATURE);
        let feedback = match crate::residency_feedback::device_feedback_mode(device) {
            crate::residency_feedback::FeedbackMode::Off => None,
            mode => Some(crate::residency_feedback::ResidencyFeedback::new(
                device, capacity, mode,
            )),
        };
        let clamp = |label, filter, mipmap_filter, anisotropy_clamp| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some(label),
                mag_filter: filter,
                min_filter: filter,
                mipmap_filter,
                anisotropy_clamp,
                ..Default::default()
            })
        };
        let clamp_samplers = [
            clamp(
                "material clamp sampler",
                wgpu::FilterMode::Linear,
                wgpu::MipmapFilterMode::Linear,
                anisotropy,
            ),
            clamp(
                "material point sampler",
                wgpu::FilterMode::Nearest,
                wgpu::MipmapFilterMode::Nearest,
                1,
            ),
        ];
        Some(Self {
            slots: TableSlots::new(capacity),
            reserved,
            views,
            layout,
            sampler: sampler.clone(),
            clamp_samplers,
            group: None,
            dirty: true,
            warned_full: false,
            stats: TextureTableStats {
                capacity,
                partially_bound,
                high_water: RESERVED_INDICES,
                ..TextureTableStats::default()
            },
            heap: crate::table_heap::HeapState::default(),
            assigned: vec![0; capacity as usize],
            feedback,
        })
    }

    /// The table index of `view` for texture `slot` ([`MATERIAL_TEXTURE_SLOTS`] order): a
    /// reserved index for a neutral view, a shared index for a texture another material holds,
    /// else a new one. A full table gives the slot's neutral index ([`SLOT_FALLBACK`]).
    fn acquire(&mut self, slot: usize, view: &wgpu::TextureView) -> u32 {
        if let Some(index) = self.reserved.iter().position(|r| r == view) {
            return index as u32;
        }
        match self.slots.acquire(view) {
            Some((index, fresh)) => {
                if fresh {
                    self.views[index as usize] = view.clone();
                    self.dirty = true;
                    if let Some(at) = self.assigned.get_mut(index as usize) {
                        *at = self.stats.refreshes;
                    }
                }
                index
            }
            None => {
                self.stats.overflows += 1;
                if !self.warned_full {
                    self.warned_full = true;
                    tracing::warn!(
                        target: "regia_render",
                        "material texture table full ({} views): textures past it sample their neutral view",
                        self.views.len()
                    );
                }
                SLOT_FALLBACK[slot]
            }
        }
    }

    /// The indices of a material's `views` (bind order), holding each.
    pub(crate) fn acquire_set(
        &mut self,
        views: &[wgpu::TextureView; MATERIAL_TEXTURE_SLOTS],
    ) -> TableIndices {
        let mut indices = TableIndices::FALLBACK;
        for (slot, view) in views.iter().enumerate() {
            indices.0[slot] = self.acquire(slot, view);
        }
        self.stats.live = self.slots.live() as u64;
        self.stats.high_water = self.slots.high_water();
        indices
    }

    /// Drops a material's holds. A texture no material holds any more leaves the table (its
    /// index shows white from the next rebuild).
    pub(crate) fn release_set(&mut self, indices: TableIndices) {
        for index in indices.0 {
            if self.slots.release(index) {
                self.views[index as usize] = self.reserved[0].clone();
                self.dirty = true;
            }
        }
        self.stats.live = self.slots.live() as u64;
        self.stats.high_water = self.slots.high_water();
    }

    /// A residency swap (ADR-1110): `views` are the material's views after the swap and `old`
    /// the indices its record names. A slot whose view did not change keeps its index and its
    /// hold. A swapped texture only this slot holds is replaced at its own index, so the record
    /// keeps naming it; any other swapped slot takes an index for the new view and drops the old
    /// one. Returns the indices the record must name (equal to `old` when every swap was in
    /// place, so the caller writes nothing).
    pub(crate) fn swap_set(
        &mut self,
        old: TableIndices,
        views: &[wgpu::TextureView; MATERIAL_TEXTURE_SLOTS],
    ) -> TableIndices {
        let mut indices = old;
        for (slot, view) in views.iter().enumerate() {
            let index = old.0[slot];
            if self.views.get(index as usize) == Some(view) {
                continue;
            }
            let in_place = index >= RESERVED_INDICES
                && !self.reserved.contains(view)
                && !self.slots.holds(view)
                && self.slots.rekey(index, view);
            if in_place {
                self.views[index as usize] = view.clone();
                self.dirty = true;
                self.stats.in_place_swaps += 1;
            } else {
                indices.0[slot] = self.acquire(slot, view);
                self.release_one(index);
                self.stats.moved_swaps += 1;
            }
        }
        self.stats.live = self.slots.live() as u64;
        self.stats.high_water = self.slots.high_water();
        indices
    }

    /// Drops one hold on `index` ([`Self::release_set`] for a single slot).
    fn release_one(&mut self, index: u32) {
        if self.slots.release(index) {
            self.views[index as usize] = self.reserved[0].clone();
            self.dirty = true;
        }
    }

    /// Rebuilds the table group when a texture entered or left since the last refresh (at most
    /// once per call; the renderer calls it once per frame, before its passes).
    pub(crate) fn refresh(&mut self, device: &wgpu::Device) {
        self.stats.refreshes += 1;
        if !self.dirty && self.group.is_some() {
            return;
        }
        // The growth policy (ADR-1110): a partially bound device binds the prefix that holds
        // every live index; any other device binds every element.
        let high_water = self.slots.high_water();
        let bound = self
            .heap
            .rebuild(high_water, self.stats.capacity, self.stats.partially_bound)
            .clamp(1, self.views.len().max(1) as u32) as usize;
        let _span = tracing::info_span!("texture_table_rebuild", views = bound).entered();
        let started = std::time::Instant::now();
        let views: Vec<&wgpu::TextureView> = self.views.iter().take(bound).collect();
        let mut entries = vec![
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureViewArray(&views),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&self.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: wgpu::BindingResource::Sampler(&self.clamp_samplers[0]),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: wgpu::BindingResource::Sampler(&self.clamp_samplers[1]),
            },
        ];
        if let Some(feedback) = &self.feedback {
            entries.push(wgpu::BindGroupEntry {
                binding: crate::residency_feedback::FEEDBACK_BINDING,
                resource: feedback.binding(),
            });
        }
        self.group = Some(crate::device::TrackedDevice::observe(device).create_bind_group(
            &wgpu::BindGroupDescriptor {
                label: Some("material texture table"),
                layout: &self.layout,
                entries: &entries,
            },
        ));
        self.dirty = false;
        self.stats.rebuilds += 1;
        self.stats.bound = bound as u32;
        self.stats.high_water = high_water;
        self.stats.bound_changes = self.heap.changes;
        self.stats.views_written = self.heap.written;
        self.stats.last_rebuild_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    }

    /// The table group of the last [`Self::refresh`].
    #[must_use]
    pub(crate) fn group(&self) -> Option<&wgpu::BindGroup> {
        self.group.as_ref()
    }

    /// Capacity, holders, overflows and rebuilds.
    #[must_use]
    pub(crate) fn stats(&self) -> TextureTableStats {
        self.stats
    }

    /// One main-view frame's residency feedback, before streaming
    /// (`crate::residency_feedback`): the newest readout with stale words dropped and how many
    /// were, or `None` (feedback off, or nothing mapped yet). Also copies the last frame's words
    /// for a later frame.
    pub(crate) fn feedback_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
    ) -> Option<(crate::residency_feedback::FeedbackReadout, u64)> {
        let tag = self.stats.refreshes;
        let mut readout = self.feedback.as_mut()?.frame(device, queue, tag)?;
        let stale = readout.drop_stale(&self.assigned);
        Some((readout, stale))
    }

    /// The feedback state (`None` while feedback is off).
    pub(crate) fn feedback_mut(&mut self) -> Option<&mut crate::residency_feedback::ResidencyFeedback> {
        self.feedback.as_mut()
    }

    /// The feedback state (`None` while feedback is off).
    #[must_use]
    pub(crate) fn feedback(&self) -> Option<&crate::residency_feedback::ResidencyFeedback> {
        self.feedback.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`TextureTable::refresh`] binds this length: 256 at high-water 4 on a partially
    /// bound device, and the whole capacity otherwise (ADR-1110).
    #[test]
    fn refresh_binds_a_256_prefix_when_partial_and_the_capacity_otherwise() {
        use crate::table_heap::HeapState;
        let mut partial = HeapState::default();
        assert_eq!(partial.rebuild(4, 4096, true), 256);
        let mut full = HeapState::default();
        assert_eq!(full.rebuild(4, 4096, false), 4096);
    }

    fn table_ready() -> (wgpu::Features, wgpu::Limits) {
        let limits = wgpu::Limits {
            max_storage_buffers_per_shader_stage: 1_048_576,
            max_storage_buffer_binding_size: 2_147_483_644,
            max_binding_array_elements_per_shader_stage: 500_000,
            max_bind_groups: 8,
            ..wgpu::Limits::default()
        };
        (TABLE_FEATURES, limits)
    }

    #[test]
    fn a_capable_device_gets_the_capped_table() {
        let (features, limits) = table_ready();
        assert_eq!(
            table_support(features, &limits, wgpu::Backend::Vulkan, true),
            TableSupport::Table {
                capacity: TEXTURE_TABLE_MAX
            }
        );
        // Metal and DX12 qualify without `PARTIALLY_BOUND_BINDING_ARRAY` (every element is bound).
        for backend in [wgpu::Backend::Metal, wgpu::Backend::Dx12] {
            assert!(
                table_support(features, &limits, backend, true)
                    .capacity()
                    .is_some()
            );
        }
    }

    /// The descriptor budget: the headroom comes off the device's binding-array limit, and the
    /// table never exceeds [`TEXTURE_TABLE_MAX`].
    #[test]
    fn capacity_leaves_headroom_under_the_binding_array_limit() {
        let at = |elements: u32| {
            table_capacity(&wgpu::Limits {
                max_binding_array_elements_per_shader_stage: elements,
                ..wgpu::Limits::default()
            })
        };
        assert_eq!(at(0), 0);
        assert_eq!(at(TEXTURE_TABLE_HEADROOM), 0);
        assert_eq!(at(4096), 4096 - TEXTURE_TABLE_HEADROOM);
        assert_eq!(at(4096 + TEXTURE_TABLE_HEADROOM), TEXTURE_TABLE_MAX);
        assert_eq!(at(u32::MAX), TEXTURE_TABLE_MAX);
        let (features, mut limits) = table_ready();
        limits.max_binding_array_elements_per_shader_stage = crate::bindless::TEXTURE_ARRAY_MIN_ELEMENTS;
        assert_eq!(
            table_support(features, &limits, wgpu::Backend::Vulkan, true).capacity(),
            Some(crate::bindless::TEXTURE_ARRAY_MIN_ELEMENTS - TEXTURE_TABLE_HEADROOM)
        );
    }

    /// Each requirement, removed alone from a capable device, names itself.
    #[test]
    fn each_missing_requirement_names_its_fallback() {
        let (features, limits) = table_ready();
        let vulkan = wgpu::Backend::Vulkan;
        let reason = |features: wgpu::Features, limits: &wgpu::Limits, backend, ssbo| {
            table_support(features, limits, backend, ssbo).fallback()
        };
        assert_eq!(
            reason(features, &limits, vulkan, false),
            Some(TableFallback::UniformMaterials)
        );
        assert_eq!(
            reason(features, &limits, wgpu::Backend::Gl, true),
            Some(TableFallback::Backend)
        );
        assert_eq!(
            reason(
                features - wgpu::Features::TEXTURE_BINDING_ARRAY,
                &limits,
                vulkan,
                true
            ),
            Some(TableFallback::NoBindingArrays)
        );
        assert_eq!(
            reason(
                features - wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING,
                &limits,
                vulkan,
                true
            ),
            Some(TableFallback::NoNonUniformIndexing)
        );
        let few_elements = wgpu::Limits {
            max_binding_array_elements_per_shader_stage: crate::bindless::TEXTURE_ARRAY_MIN_ELEMENTS - 1,
            ..limits.clone()
        };
        assert_eq!(
            reason(features, &few_elements, vulkan, true),
            Some(TableFallback::ArrayElements)
        );
        let four_groups = wgpu::Limits {
            max_bind_groups: 4,
            ..limits.clone()
        };
        assert_eq!(
            reason(features, &four_groups, vulkan, true),
            Some(TableFallback::BindGroups)
        );
        // The WebGPU defaults have none of it.
        assert_eq!(
            reason(wgpu::Features::empty(), &wgpu::Limits::default(), vulkan, true),
            Some(TableFallback::NoBindingArrays)
        );
    }

    #[test]
    fn fallback_names_are_stable_protocol_strings() {
        let all = [
            (TableFallback::NotRequested, "not_requested"),
            (TableFallback::UniformMaterials, "uniform_materials"),
            (TableFallback::Backend, "backend"),
            (TableFallback::NoBindingArrays, "no_binding_arrays"),
            (TableFallback::NoNonUniformIndexing, "no_non_uniform_indexing"),
            (TableFallback::ArrayElements, "array_elements"),
            (TableFallback::BindGroups, "bind_groups"),
        ];
        for (reason, name) in all {
            assert_eq!(reason.as_str(), name);
            assert_eq!(reason.to_string(), name);
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
        }
    }

    #[test]
    fn the_views_setting_parses_and_clamps() {
        let min = crate::bindless::TEXTURE_ARRAY_MIN_ELEMENTS;
        assert_eq!(requested_views(None), TEXTURE_TABLE_MAX);
        assert_eq!(requested_views(Some("")), TEXTURE_TABLE_MAX);
        assert_eq!(requested_views(Some("lots")), TEXTURE_TABLE_MAX);
        assert_eq!(requested_views(Some(" 16384 ")), 16_384);
        assert_eq!(
            requested_views(Some("12")),
            min,
            "never below the tier's array minimum"
        );
        assert_eq!(
            requested_views(Some("1000000")),
            TEXTURE_TABLE_VIEWS_LIMIT,
            "16-bit indices"
        );
        let limits = wgpu::Limits {
            max_binding_array_elements_per_shader_stage: 20_000,
            ..wgpu::Limits::default()
        };
        assert_eq!(table_capacity_with(&limits, 16_384), 16_384);
        assert_eq!(
            table_capacity_with(&limits, TEXTURE_TABLE_VIEWS_LIMIT),
            20_000 - TEXTURE_TABLE_HEADROOM,
            "the device's budget still caps it"
        );
        // The largest index the record can name is the last one of the largest table.
        assert_eq!(
            TableIndices([TEXTURE_TABLE_VIEWS_LIMIT - 1; 7], 0).words()[3],
            TEXTURE_TABLE_VIEWS_LIMIT - 1
        );
    }

    #[test]
    fn every_fallback_has_a_unique_code_and_a_message() {
        let mut codes = std::collections::HashSet::new();
        for reason in TableFallback::ALL {
            assert!(codes.insert(reason.as_str()), "duplicate code {reason}");
            assert!(
                reason
                    .as_str()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{reason}"
            );
            assert!(!reason.message().is_empty(), "{reason}");
            assert!(
                !reason.message().ends_with('.'),
                "messages are fragments: {reason}"
            );
            if let Some(hint) = reason.hint() {
                assert!(!hint.is_empty(), "{reason}");
            }
        }
        assert_eq!(codes.len(), TableFallback::ALL.len());
        // A match with no wildcard: a new variant fails to compile here until ALL lists it.
        for reason in TableFallback::ALL {
            match reason {
                TableFallback::NotRequested
                | TableFallback::UniformMaterials
                | TableFallback::Backend
                | TableFallback::NoBindingArrays
                | TableFallback::NoNonUniformIndexing
                | TableFallback::ArrayElements
                | TableFallback::BindGroups => {}
            }
        }
    }

    #[test]
    fn the_level_setting_parses_and_caps() {
        assert_eq!(requested_level(None), DEFAULT_BINDLESS_LEVEL);
        assert_eq!(requested_level(Some("")), DEFAULT_BINDLESS_LEVEL);
        assert_eq!(requested_level(Some("three")), DEFAULT_BINDLESS_LEVEL);
        assert_eq!(requested_level(Some("-1")), DEFAULT_BINDLESS_LEVEL);
        assert_eq!(requested_level(Some("0")), 0);
        assert_eq!(requested_level(Some(" 3 ")), 3);
        assert_eq!(requested_level(Some("9")), MAX_BINDLESS_LEVEL);
        // The default does not reach the table until its acceptance passes on the gate.
        const { assert!(DEFAULT_BINDLESS_LEVEL < TEXTURE_TABLE_LEVEL) };
    }

    /// Features are requested only at the table level and only when the adapter has both.
    #[test]
    fn the_device_requests_table_features_only_at_level_three() {
        let all = TABLE_FEATURES | wgpu::Features::PARTIALLY_BOUND_BINDING_ARRAY;
        assert_eq!(device_features_for_level(2, all), wgpu::Features::empty());
        assert_eq!(device_features_for_level(3, all), TABLE_FEATURES);
        assert_eq!(device_features_for_level(4, all), TABLE_FEATURES);
        assert_eq!(
            device_features_for_level(3, wgpu::Features::TEXTURE_BINDING_ARRAY),
            wgpu::Features::empty()
        );
        assert_eq!(
            device_features_for_level(3, wgpu::Features::empty()),
            wgpu::Features::empty()
        );
    }

    #[test]
    fn layouts_gain_the_table_group_only_on_the_table_path() {
        // Without a table the prefix comes back unchanged (the padding needs a real layout, so
        // the frame tests cover it).
        let none: Vec<Option<&wgpu::BindGroupLayout>> = with_table(&[None, None, None], None);
        assert_eq!(none.len(), 3);
        let entries = table_layout_entries(4032);
        assert_eq!(entries[0].count.map(std::num::NonZeroU32::get), Some(4032));
        assert_eq!(entries[0].visibility, wgpu::ShaderStages::VERTEX_FRAGMENT);
        assert!(entries[1].count.is_none(), "the sampler is a single binding");
        // A zero capacity never reaches a layout (the gate refuses it), but the count stays
        // non-zero so the entry is well-formed.
        assert_eq!(
            table_layout_entries(0)[0].count.map(std::num::NonZeroU32::get),
            Some(1)
        );
    }

    #[test]
    fn a_table_that_overflowed_is_a_frame_problem() {
        let mut stats = TextureTableStats::default();
        assert!(table_problem(&stats).is_none(), "every texture fitted");
        stats.capacity = 4032;
        stats.live = 4028;
        stats.overflows = 3;
        let problem = table_problem(&stats).expect("an overflow is a problem");
        assert_eq!(problem.code, PROBLEM_TEXTURE_TABLE_FULL);
        assert!(problem.message.contains("4032 views"), "{}", problem.message);
        assert!(
            PROBLEM_TEXTURE_TABLE_FULL
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'_')
        );
    }

    /// Every bind of a material group (slot 2) passes the entry's dynamic offsets: on the table
    /// path the shared record group needs its offset, and a bind with `&[]` would be a
    /// validation error at level 3 only, which the default suite never runs (ADR-863).
    #[test]
    fn every_material_bind_site_passes_its_offsets() {
        for (file, source) in [
            ("frame.rs", include_str!("frame.rs")),
            (
                "frame/translucent_shadow.rs",
                include_str!("frame/translucent_shadow.rs"),
            ),
        ] {
            for (number, line) in source.lines().enumerate() {
                let line = line.trim();
                let binds_material = line.contains("set_bind_group(") && line.contains(", 2, ");
                assert!(
                    !(binds_material && line.contains("bg, &[])")),
                    "{file}:{}: a material group bound without its offsets: {line}",
                    number + 1
                );
            }
        }
    }

    #[test]
    fn indices_pack_two_per_word_in_slot_order() {
        let words = TableIndices([4, 5, 6, 7, 8, 9, 10], 0).words();
        assert_eq!(words, [4 | (5 << 16), 6 | (7 << 16), 8 | (9 << 16), 10]);
        assert_eq!(
            TableIndices::FALLBACK.words(),
            [1 << 16, 0, 2 | (1 << 16), 3],
            "neutral indices: white, flat, white, white, mid, flat, aux"
        );
        let top = TEXTURE_TABLE_MAX - 1;
        assert_eq!(
            TableIndices([top; 7], 0).words()[3],
            top,
            "the largest index fits 16 bits"
        );
    }

    #[test]
    fn shared_keys_share_an_index_and_free_on_the_last_release() {
        let mut slots: TableSlots<u32> = TableSlots::new(16);
        let (a, fresh_a) = slots.acquire(&100).expect("room");
        assert!(fresh_a);
        assert_eq!(
            a, RESERVED_INDICES,
            "the first handed-out index follows the reserved ones"
        );
        let (again, fresh_again) = slots.acquire(&100).expect("room");
        assert_eq!((again, fresh_again), (a, false), "a held key is shared");
        assert_eq!(slots.refs(a), 2);
        let (b, _) = slots.acquire(&200).expect("room");
        assert_ne!(a, b);
        assert_eq!(slots.live(), 2);
        assert!(!slots.release(a), "one holder left");
        assert!(slots.release(a), "the last holder frees it");
        assert_eq!(slots.refs(a), 0);
        assert!(!slots.release(a), "a free index releases nothing");
        assert!(!slots.release(0), "reserved indices are never released");
        assert!(!slots.release(999), "unknown indices are ignored");
        let (c, fresh_c) = slots.acquire(&300).expect("room");
        assert_eq!((c, fresh_c), (a, true), "a freed index is reused");
        assert_eq!(slots.live(), 2);
    }

    /// A full table refuses new keys without changing anything, still shares held keys, and
    /// takes new ones again after a release.
    #[test]
    fn a_full_table_refuses_without_side_effects() {
        let capacity = RESERVED_INDICES + 3;
        let mut slots: TableSlots<u32> = TableSlots::new(capacity);
        let held: Vec<u32> = (0..3).map(|k| slots.acquire(&k).expect("room").0).collect();
        assert_eq!(held, vec![4, 5, 6]);
        assert_eq!(slots.acquire(&99), None);
        assert_eq!(slots.live(), 3);
        assert_eq!(slots.acquire(&1), Some((5, false)), "a held key still shares");
        assert!(!slots.release(5));
        assert!(slots.release(5));
        assert_eq!(slots.acquire(&99), Some((5, true)));
        assert!(held.iter().all(|&i| i < capacity));
    }

    /// Churn: 10 000 rounds of acquiring and releasing a sliding window of keys never grow the
    /// handed-out range past the peak live count.
    #[test]
    fn churn_stays_within_the_peak() {
        let window = 32u32;
        let mut slots: TableSlots<u32> = TableSlots::new(RESERVED_INDICES + window);
        let mut held = std::collections::VecDeque::new();
        for key in 0..10_000u32 {
            if held.len() == window as usize
                && let Some(index) = held.pop_front()
            {
                assert!(slots.release(index));
            }
            let (index, fresh) = slots.acquire(&key).expect("never past the window");
            assert!(fresh);
            assert!((RESERVED_INDICES..RESERVED_INDICES + window).contains(&index));
            held.push_back(index);
        }
        assert_eq!(slots.live(), window as usize);
    }
}

/// The table variants of every standard-material shader compose and validate (naga, no GPU),
/// and their bindings are exactly what the table path's layouts hold: group 2 the record alone,
/// group 4 the table and its sampler set (ADR-863, ADR-2071).
#[cfg(test)]
mod shader_tests {
    use super::*;

    /// The table size the tests compose with (a lavapipe-sized table).
    const SIZE: u32 = TEXTURE_TABLE_MAX - TEXTURE_TABLE_HEADROOM;

    fn compose(source: &str, file: &str, extra: &[&str], table: bool) -> naga::Module {
        let mut defs: HashMap<String, ShaderDefValue> = extra
            .iter()
            .map(|name| ((*name).to_string(), ShaderDefValue::Bool(true)))
            .collect();
        defs.insert(
            crate::bindless::MATERIAL_SSBO_DEF.to_string(),
            ShaderDefValue::Bool(true),
        );
        if table {
            defs.insert(TEXTURE_TABLE_DEF.to_string(), ShaderDefValue::Bool(true));
            defs.insert(TEXTURE_TABLE_SIZE_DEF.to_string(), ShaderDefValue::UInt(SIZE));
        }
        crate::shader_lib::compose_source(source, file, defs)
            .unwrap_or_else(|err| panic!("{file} {extra:?} table {table}: {err}"))
    }

    fn validate(module: &naga::Module, what: &str) {
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(module)
        .unwrap_or_else(|err| panic!("{what}: {err:?}"));
    }

    /// `(binding, type)` of every global in `group`, sorted by binding.
    fn group_bindings(module: &naga::Module, group: u32) -> Vec<(u32, naga::TypeInner)> {
        let mut found: Vec<(u32, naga::TypeInner)> = module
            .global_variables
            .iter()
            .filter_map(|(_, var)| {
                let binding = var.binding.as_ref().filter(|b| b.group == group)?;
                Some((binding.binding, module.types[var.ty].inner.clone()))
            })
            .collect();
        found.sort_by_key(|(binding, _)| *binding);
        found
    }

    fn cases() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
        let forward = include_str!("../shaders/lab_forward.wgsl");
        let caster = include_str!("../shaders/translucent_shadow.wgsl");
        let resolve = crate::visbuffer::gpu::resolve_source();
        vec![
            (forward, "lab_forward.wgsl", &["ALPHA_TEST"]),
            (
                forward,
                "lab_forward.wgsl",
                &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"],
            ),
            (forward, "lab_forward.wgsl", &["GPU_DRIVEN", "ALPHA_TEST"]),
            (forward, "lab_forward.wgsl", &["GLASS", "ALPHA_TEST"]),
            (forward, "lab_forward.wgsl", &["MASK_DECALS", "ALPHA_TEST"]),
            (resolve, "vis_resolve.wgsl", &["VISBUFFER"]),
            (caster, "translucent_shadow.wgsl", &[]),
            (caster, "translucent_shadow.wgsl", &["GLASS"]),
            (
                include_str!("../shaders/lab_shadow.wgsl"),
                "lab_shadow.wgsl",
                &["GPU_DRIVEN", "ALPHA_TEST"],
            ),
        ]
    }

    #[test]
    fn table_variants_validate_and_bind_the_record_and_the_table_only() {
        for (source, file, extra) in cases() {
            let what = format!("{file} {extra:?}");
            let module = compose(source, file, extra, true);
            validate(&module, &what);
            let material = group_bindings(&module, 2);
            assert_eq!(
                material.iter().map(|(binding, _)| *binding).collect::<Vec<_>>(),
                vec![0],
                "{what}: group 2 holds the record alone on the table path"
            );
            let table = group_bindings(&module, TEXTURE_TABLE_GROUP);
            assert_eq!(table.len(), 4, "{what}: the table and the shared sampler set");
            for (position, binding) in [(2, 3), (3, 4)] {
                assert!(
                    matches!(table[position], (b, naga::TypeInner::Sampler { .. }) if b == binding),
                    "{what}: binding {binding} of the table group is a shared sampler"
                );
            }
            match &table[0] {
                (0, naga::TypeInner::BindingArray { size, .. }) => assert_eq!(
                    *size,
                    naga::ArraySize::Constant(std::num::NonZeroU32::new(SIZE).expect("non-zero")),
                    "{what}: the table holds TEXTURE_TABLE_SIZE views"
                ),
                other => panic!("{what}: binding 0 of the table group is {other:?}"),
            }
            assert!(
                matches!(table[1], (1, naga::TypeInner::Sampler { .. })),
                "{what}: binding 1 of the table group is the material sampler"
            );
        }
    }

    /// The same shaders without the table keep the per-material bindings and never declare
    /// group 4, so devices off the table path compose exactly what they did before.
    #[test]
    fn variants_off_the_table_declare_no_table_group() {
        for (source, file, extra) in cases() {
            let what = format!("{file} {extra:?}");
            let module = compose(source, file, extra, false);
            validate(&module, &what);
            assert!(
                group_bindings(&module, TEXTURE_TABLE_GROUP).is_empty(),
                "{what}: no table group off the table path"
            );
            assert!(
                group_bindings(&module, 2).len() > 1,
                "{what}: the per-material textures stay in group 2"
            );
        }
    }

    /// The descriptor budget: Vulkan counts every sampled image of a stage against the
    /// update-after-bind limit that bounds the table, so the other textures a table variant
    /// declares (frame, refraction, glass, mask-decal groups) must fit [`TEXTURE_TABLE_HEADROOM`].
    #[test]
    fn the_other_textures_fit_the_headroom() {
        for (source, file, extra) in cases() {
            let module = compose(source, file, extra, true);
            let others: u32 = module
                .global_variables
                .iter()
                .filter(|(_, var)| {
                    var.binding
                        .as_ref()
                        .is_some_and(|b| b.group != TEXTURE_TABLE_GROUP)
                })
                .map(|(_, var)| match &module.types[var.ty].inner {
                    naga::TypeInner::Image { .. } => 1,
                    naga::TypeInner::BindingArray { size, .. } => match size {
                        naga::ArraySize::Constant(count) => count.get(),
                        _ => TEXTURE_TABLE_HEADROOM,
                    },
                    _ => 0,
                })
                .sum();
            assert!(
                others <= TEXTURE_TABLE_HEADROOM,
                "{file} {extra:?}: {others} other sampled textures, headroom {TEXTURE_TABLE_HEADROOM}"
            );
        }
    }

    /// The record the shaders read on the table path is the renderer's: the constants, then one
    /// `vec4<u32>` of indices (the arena's `record` and the layout's `min_binding_size`).
    #[test]
    fn the_shader_record_is_the_arena_record() {
        let module = compose(
            include_str!("../shaders/lab_forward.wgsl"),
            "lab_forward.wgsl",
            &["ALPHA_TEST"],
            true,
        );
        let Some((_, var)) = module.global_variables.iter().find(|(_, var)| {
            var.binding
                .as_ref()
                .is_some_and(|b| b.group == 2 && b.binding == 0)
        }) else {
            panic!("no material record binding");
        };
        let size = module.types[var.ty].inner.size(module.to_ctx());
        let constants = (crate::surface_graph::GRAPH_STANDARD_VEC4S * 16) as u32;
        assert_eq!(u64::from(size), u64::from(constants) + TABLE_INDEX_BYTES);
    }
}

/// GPU checks of the table itself (lavapipe; they skip without an adapter that has the
/// features): a fully bound table builds without `PARTIALLY_BOUND_BINDING_ARRAY`, and refreshes
/// rebuild only after a change.
#[cfg(test)]
mod gpu_tests {
    use super::*;

    /// A device with the table features on the default adapter, or `None` (skip).
    pub(crate) fn table_device() -> Option<crate::RenderDevice> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = crate::device::instance_backends();
        descriptor.flags = crate::device::instance_flags();
        let instance = crate::device::create_instance(descriptor);
        let adapter = crate::gpu_select::select(&instance)
            .0
            .or_else(|| crate::gpu_select::default_adapter(&instance, None))?;
        let features = device_features_for_level(TEXTURE_TABLE_LEVEL, adapter.features());
        if features.is_empty() {
            return None;
        }
        let (device, queue) = regia_core::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("texture table test"),
            required_limits: adapter.limits(),
            required_features: features,
            memory_hints: crate::device::device_memory_hints(&adapter.get_info()),
            ..Default::default()
        }))
        .ok()?;
        let capabilities = crate::RenderCapabilities::from_adapter(&adapter, &device);
        Some(crate::RenderDevice::from_parts(
            instance,
            adapter,
            device,
            queue,
            capabilities,
        ))
    }

    fn solid(device: &wgpu::Device, label: &str) -> wgpu::TextureView {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default())
    }

    #[test]
    fn a_fully_bound_table_builds_and_rebuilds_only_after_changes() {
        let Some(render) = table_device() else {
            eprintln!("skipping: no adapter with texture binding arrays");
            return;
        };
        let device = &render.device;
        let Some(capacity) = device_support(device).capacity() else {
            eprintln!(
                "skipping: the device stays off the table ({:?})",
                device_support(device)
            );
            return;
        };
        let reserved = [
            solid(device, "white"),
            solid(device, "flat"),
            solid(device, "mid"),
            solid(device, "aux"),
        ];
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut table = TextureTable::new(
            device,
            [&reserved[0], &reserved[1], &reserved[2], &reserved[3]],
            &sampler,
            1,
        )
        .expect("table path");
        assert_eq!(table.stats().capacity, capacity);
        table.refresh(device);
        assert_eq!(table.stats().rebuilds, 1, "the first refresh builds the group");
        table.refresh(device);
        assert_eq!(table.stats().rebuilds, 1, "nothing changed: no rebuild");

        // A material with two own textures and five neutral slots.
        let albedo = solid(device, "albedo");
        let orm = solid(device, "orm");
        let views = [
            albedo.clone(),
            reserved[1].clone(),
            orm.clone(),
            reserved[0].clone(),
            reserved[2].clone(),
            reserved[1].clone(),
            reserved[3].clone(),
        ];
        let first = table.acquire_set(&views);
        assert_eq!(first.0, [4, 1, 5, 0, 2, 1, 3]);
        // A second material sharing the base colour: no new index, no rebuild needed for it.
        let second = table.acquire_set(&views);
        assert_eq!(first, second);
        assert_eq!(table.stats().live, 2);
        table.refresh(device);
        table.refresh(device);
        assert_eq!(
            table.stats().rebuilds,
            2,
            "one rebuild for the frame that added textures"
        );
        table.release_set(first);
        table.refresh(device);
        assert_eq!(table.stats().rebuilds, 2, "still held by the second material");
        table.release_set(second);
        table.refresh(device);
        assert_eq!(table.stats().rebuilds, 3, "the textures left the table");
        assert_eq!(table.stats().live, 0);
        assert_eq!(table.stats().refreshes, 6);
        assert_eq!(table.stats().overflows, 0);
        assert!(table.group().is_some());
        let error = regia_core::tasks::block_on(scope.pop());
        assert!(error.is_none(), "validation error: {error:?}");
    }
}

/// Frame-level checks of the table path (lavapipe; they skip without an adapter that has the
/// table features): the same frames as the per-material path, byte for byte, with one material
/// group per pass, and one shared record group however many materials come and go (ADR-863).
#[cfg(test)]
pub(crate) mod frame_tests {
    use super::*;
    use crate::{
        ALPHA_BLEND, CpuMesh, DirectionalLightUniform, FrameMesh, FrameQuality, FrameRenderer, FrameScene,
        LabMaterial, LabMesh, LabScene, LabTexture, LabVertex, RenderDevice,
    };

    /// A device on `base`'s adapter with `base`'s limits and features, plus the table features
    /// when `table` is set and without them otherwise. The two devices of a pair differ in those
    /// features only (so in their material path), and both carry the engine's other optional
    /// features (GPU-driven draws, timestamps, compressed textures) as `base` has them.
    fn device(base: &RenderDevice, table: bool) -> Option<RenderDevice> {
        let table_features = device_features_for_level(TEXTURE_TABLE_LEVEL, base.adapter.features());
        if table_features.is_empty() {
            return None;
        }
        let features = if table {
            base.device.features() | table_features
        } else {
            base.device.features() - table_features
        };
        let (device, queue) =
            regia_core::tasks::block_on(base.adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("texture table frame test"),
                required_limits: base.device.limits(),
                required_features: features,
                memory_hints: crate::device::device_memory_hints(&base.adapter.get_info()),
                ..Default::default()
            }))
            .ok()?;
        let capabilities = crate::RenderCapabilities::from_adapter(&base.adapter, &device);
        Some(RenderDevice::from_parts(
            base.instance.clone(),
            base.adapter.clone(),
            device,
            queue,
            capabilities,
        ))
    }

    /// The table device and its per-material twin, or `None` (skip) when the adapter cannot
    /// carry the table. One pair per test process, shared by the tests (like
    /// `RenderDevice::shared_headless`: test threads that each open devices can livelock the
    /// NVIDIA driver).
    pub(crate) fn pair() -> Option<(RenderDevice, RenderDevice)> {
        static PAIR: std::sync::OnceLock<Option<(RenderDevice, RenderDevice)>> = std::sync::OnceLock::new();
        PAIR.get_or_init(open_pair).clone()
    }

    fn open_pair() -> Option<(RenderDevice, RenderDevice)> {
        let base = RenderDevice::shared_headless().ok()?;
        let table = device(&base, true)?;
        if device_support(&table.device).capacity().is_none() {
            eprintln!(
                "skipping: the device stays off the table ({:?})",
                device_support(&table.device)
            );
            return None;
        }
        let classic = device(&base, false)?;
        assert!(
            device_support(&classic.device).capacity().is_none(),
            "a device without the table features keeps the per-material bindings"
        );
        Some((table, classic))
    }

    fn quad(x: f32, z: f32, material: usize) -> LabMesh {
        LabMesh {
            vertices: vec![
                LabVertex::from_pos_normal_uv([x - 0.3, -0.3, z], [0.0, 0.0, 1.0], [0.0, 1.0]),
                LabVertex::from_pos_normal_uv([x + 0.3, -0.3, z], [0.0, 0.0, 1.0], [1.0, 1.0]),
                LabVertex::from_pos_normal_uv([x + 0.3, 0.3, z], [0.0, 0.0, 1.0], [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([x - 0.3, 0.3, z], [0.0, 0.0, 1.0], [0.0, 0.0]),
            ],
            indices: vec![0, 1, 2, 0, 2, 3],
            material,
            ..LabMesh::unskinned()
        }
    }

    fn scene(materials: Vec<LabMaterial>, meshes: Vec<LabMesh>, shadows: bool) -> LabScene {
        LabScene {
            width: 64,
            height: 32,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 20.0,
            depth_prepass: true,
            directional_shadows: shadows,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 5_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials,
            meshes,
        }
    }

    /// A 2 × 2 texture of `a` and `b` in a checker.
    fn checker(a: [u8; 4], b: [u8; 4]) -> LabTexture {
        let mut rgba = Vec::with_capacity(16);
        for texel in [a, b, b, a] {
            rgba.extend_from_slice(&texel);
        }
        LabTexture {
            width: 2,
            height: 2,
            rgba,
        }
    }

    fn tinted(r: f32, roughness: f32) -> LabMaterial {
        LabMaterial {
            base_color: [r, 0.5, 0.25, 1.0],
            roughness,
            ..LabMaterial::default()
        }
    }

    /// The acceptance check that needs no golden: textured, normal-mapped, emissive, untextured
    /// and blended materials, sun shadows (so the translucent casters sample through the table
    /// too) and three frames of parameter edits draw the same bytes on the table path as on the
    /// per-material path. Each table-path pass binds one material group (the record group at
    /// per-material offsets), where the per-material path binds one per material run, and the
    /// offset changes equal the per-material path's group changes.
    #[test]
    fn table_and_per_material_paths_draw_identical_pixels() {
        // Every frame complete (ADR-1584): while another test of this binary runs a warm queue
        // with workers, a deferred variant (the glass quad's blend pipeline) not built yet would
        // skip its draw for a frame on one path and not the other.
        let _complete = crate::pipeline_warm::blocking_scope();
        let Some((table, classic)) = pair() else {
            eprintln!("skipping: no adapter with the texture table features");
            return;
        };
        let white = [255, 255, 255, 255];
        let dark = [40, 40, 40, 255];
        let textured = LabMaterial {
            albedo: Some(checker(white, dark)),
            emissive_map: Some(checker(dark, white)),
            emissive: [0.2, 0.1, 0.0],
            ..tinted(0.9, 0.3)
        };
        let mapped = LabMaterial {
            albedo: Some(checker([200, 120, 60, 255], white)),
            normal_map: Some(checker([150, 128, 230, 255], [110, 128, 230, 255])),
            occlusion_map: Some(checker([255, 200, 90, 255], [180, 120, 40, 255])),
            ..tinted(0.4, 0.6)
        };
        let glass = LabMaterial {
            base_color: [0.3, 0.6, 0.9, 0.5],
            albedo: Some(checker(white, [160, 160, 255, 255])),
            alpha_mode: ALPHA_BLEND,
            ..LabMaterial::default()
        };
        let mut lab = scene(
            vec![tinted(0.8, 0.4), textured, mapped, glass],
            vec![
                quad(-1.2, 0.0, 0),
                quad(-0.6, 0.0, 1),
                quad(0.0, 0.0, 2),
                quad(0.6, 0.0, 1),
                quad(1.2, 0.0, 0),
                quad(0.0, 0.5, 3),
            ],
            true,
        );
        let mut on_table = FrameRenderer::new();
        let mut on_classic = FrameRenderer::new();
        for frame in 0..3 {
            lab.materials[0].roughness = 0.3 + 0.2 * frame as f32;
            lab.materials[2].base_color[1] = 0.4 + 0.1 * frame as f32;
            let scene = lab.as_frame_scene_with_quality(FrameQuality::LAB);
            let a = on_table
                .render_offscreen(&table, &scene)
                .expect("table path frame")
                .pixels;
            let b = on_classic
                .render_offscreen(&classic, &scene)
                .expect("per-material path frame")
                .pixels;
            assert_eq!(a.len(), b.len());
            let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
            assert_eq!(
                differing, 0,
                "frame {frame}: {differing} bytes differ between the table and the per-material path"
            );
            let (t, c) = (on_table.draw_counts(), on_classic.draw_counts());
            for ((name, tp), (_, cp)) in t.passes().iter().zip(c.passes()) {
                assert_eq!(tp.draws, cp.draws, "frame {frame} {name}: draws");
                assert_eq!(
                    tp.pipeline_switches, cp.pipeline_switches,
                    "frame {frame} {name}: pipelines"
                );
                assert_eq!(
                    tp.bind_group_switches, cp.bind_group_switches,
                    "frame {frame} {name}: an offset change on the table path for each group change on the other"
                );
                if tp.draws > 0 && tp.material_batches > 0 {
                    assert_eq!(
                        tp.material_batches, 1,
                        "frame {frame} {name}: one material group per pass on the table path"
                    );
                }
            }
            assert!(
                c.opaque.material_batches > 1,
                "frame {frame}: the per-material path binds a group per material run ({:?})",
                c.opaque
            );
        }
        let stats = on_table
            .texture_table_stats()
            .expect("table stats on the table path");
        assert_eq!(stats.record_groups, 1, "one shared record group");
        assert_eq!(
            stats.rebuilds, 1,
            "every texture entered on the first frame: one rebuild"
        );
        assert!(stats.refreshes >= 3, "refreshed every frame: {stats:?}");
        assert!(
            stats.live >= 3,
            "the materials' own textures hold indices: {stats:?}"
        );
        assert_eq!(stats.overflows, 0);
        assert!(
            on_classic.texture_table_stats().is_none(),
            "no table off the table path"
        );
        let arena = on_table.material_arena_stats().expect("arena on the table path");
        assert_eq!(arena.live, 4, "every standard material holds a record");
    }

    /// Streaming slots: a streamed material's residency swaps (low mips first, then the rest
    /// under a small per-frame byte budget, then the fade) rewrite its record's table indices and
    /// rebuild the table at most once a frame. Over a fixed 40-frame sequence each frame is
    /// byte-identical to the per-material path, where every swap rebuilds the material's group.
    #[test]
    fn streamed_swaps_draw_identical_pixels_through_the_table() {
        let Some((table, classic)) = pair() else {
            eprintln!("skipping: no adapter with the texture table features");
            return;
        };
        // A `size`² pattern whose mip levels all differ.
        let pattern = |size: u32, seed: u32| {
            let mut rgba = Vec::with_capacity((size * size * 4) as usize);
            for y in 0..size {
                for x in 0..size {
                    let h = (x.wrapping_mul(73_856_093) ^ y.wrapping_mul(19_349_663) ^ seed)
                        .wrapping_mul(2_654_435_761);
                    let checker = if ((x / 8) + (y / 8)).is_multiple_of(2) {
                        200
                    } else {
                        60
                    };
                    rgba.extend_from_slice(&[checker, (h >> 24) as u8, ((x * 255) / size) as u8, 255]);
                }
            }
            LabTexture {
                width: size,
                height: size,
                rgba,
            }
        };
        let material = LabMaterial {
            albedo: Some(pattern(256, 1)),
            occlusion_map: Some(pattern(128, 2)),
            roughness: 0.6,
            ..LabMaterial::default()
        };
        let vertices: Vec<LabVertex> = [[-2.0f32, -2.0], [2.0, -2.0], [2.0, 2.0], [-2.0, 2.0]]
            .into_iter()
            .map(|[x, z]| LabVertex {
                position: [x, 0.0, z],
                normal: [0.0, 1.0, 0.0],
                uv: [x + 2.0, z + 2.0],
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect();
        let indices = [0u32, 2, 1, 0, 3, 2];
        let streaming = crate::TextureStreaming {
            enabled: true,
            initial_max_px: 16,
            max_px: 0,
            upload_kib_per_frame: 24,
            upload_us_per_frame: 1_000_000_000,
            fade_frames: 3,
            need_margin_percent: 200,
            resident_all: true,
            budget_mib: 0,
            eager: false,
        };
        let scene = FrameScene {
            width: 64,
            height: 64,
            eye: [0.3, 1.5, 0.4],
            target: [0.0, 0.0, -0.4],
            fov_y: 1.0,
            near: 0.05,
            far: 50.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 0.0,
            ambient_lux: 0.0,
            double_sided: true,
            post: None,
            fog: crate::FogSettings::default(),
            directional: DirectionalLightUniform::new([0.3, -1.0, 0.2], [1.0, 1.0, 1.0], 1.0),
            local_lights: &[],
            quality: FrameQuality {
                texture_streaming: streaming,
                ..FrameQuality::LAB
            },
            meshes: vec![FrameMesh {
                is_static: false,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &vertices,
                indices: &indices,
                material: &material,
                model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: crate::ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }],
        };
        let mut on_table = FrameRenderer::new();
        let mut on_classic = FrameRenderer::new();
        let mut first = None;
        let mut changed = false;
        for frame in 0..40 {
            let a = on_table
                .render_offscreen(&table, &scene)
                .expect("table path frame")
                .pixels;
            let b = on_classic
                .render_offscreen(&classic, &scene)
                .expect("per-material path frame")
                .pixels;
            let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
            assert_eq!(
                differing, 0,
                "frame {frame}: {differing} bytes differ while streaming"
            );
            match &first {
                None => first = Some(a),
                Some(first) => changed |= *first != a,
            }
        }
        assert!(changed, "the stream swapped residency during the sequence");
        let stats = on_table.texture_table_stats().expect("table stats");
        assert!(
            stats.rebuilds > 1,
            "residency swaps put new views in the table: {stats:?}"
        );
        assert!(
            stats.rebuilds <= stats.refreshes,
            "at most one table rebuild per frame: {stats:?}"
        );
        assert_eq!(stats.record_groups, 1, "a swap rebuilds no material group");
        assert_eq!(stats.overflows, 0);
    }

    /// Churn on the table path: 300 materials (past the arena's first 256 slots, so it grows
    /// once), then 30 keys replaced per frame for five frames. Every material binds the shared
    /// record group: the group count stays at one per arena buffer, the replaced keys were real
    /// uploads, and untextured materials put nothing in the table.
    #[test]
    fn table_path_shares_one_record_group_under_material_churn() {
        let Some((table, _)) = pair() else {
            eprintln!("skipping: no adapter with the texture table features");
            return;
        };
        const LIVE: u64 = 300;
        const PER_FRAME: u64 = 30;
        const KEY_BASE: u64 = 0x00C3_0000_0000_0000;
        const OBJECT_BASE: u64 = 0x00C4_0000_0000_0000;
        let corners = [[-0.5f32, -0.5], [0.5, -0.5], [0.5, 0.5], [-0.5, 0.5]];
        let mesh = CpuMesh {
            vertices: corners
                .iter()
                .map(|&[x, y]| {
                    LabVertex::from_pos_normal_uv([x, y, 0.0], [0.0, 0.0, 1.0], [x + 0.5, y + 0.5])
                })
                .collect(),
            indices: vec![0, 1, 2, 0, 2, 3],
        };
        let mut lab = scene(
            vec![LabMaterial::default()],
            vec![LabMesh {
                vertices: mesh.vertices.clone(),
                indices: mesh.indices.clone(),
                ..LabMesh::unskinned()
            }],
            false,
        );
        lab.depth_prepass = false;
        let base = lab.as_frame_scene_with_quality(FrameQuality::LAB);
        let template = base.meshes[0];
        let side = 18usize;
        let model = |i: usize| -> [[f32; 4]; 4] {
            let cell = 2.6 / side as f32;
            let (x, y) = ((i % side) as f32, (i / side) as f32);
            [
                [cell * 0.9, 0.0, 0.0, 0.0],
                [0.0, cell * 0.9, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [-1.3 + (x + 0.5) * cell, -1.3 + (y + 0.5) * cell, 0.0, 1.0],
            ]
        };
        let mut ids: Vec<u64> = (0..LIVE).collect();
        let mut next = LIVE;
        let mut renderer = FrameRenderer::new();
        let mut after_first = None;
        for frame in 0..6u64 {
            if frame > 0 {
                for k in 0..PER_FRAME {
                    let slot = ((frame * PER_FRAME + k) % LIVE) as usize;
                    ids[slot] = next;
                    next += 1;
                }
            }
            let materials: Vec<LabMaterial> = ids
                .iter()
                .map(|&id| tinted((id % 17) as f32 / 17.0, 0.3 + (id % 5) as f32 * 0.1))
                .collect();
            let scene = FrameScene {
                meshes: materials
                    .iter()
                    .zip(&ids)
                    .enumerate()
                    .map(|(i, (material, &id))| FrameMesh {
                        material_key: KEY_BASE | id,
                        object_key: OBJECT_BASE | i as u64,
                        material,
                        model: model(i),
                        ..template
                    })
                    .collect(),
                ..base.clone()
            };
            renderer.render_offscreen(&table, &scene).expect("churn frame");
            if frame == 0 {
                after_first = Some((
                    renderer.texture_table_stats().expect("table stats"),
                    renderer.cache_stats().material_uploads,
                ));
            }
        }
        let (first, uploads_first) = after_first.expect("first frame");
        let stats = renderer.texture_table_stats().expect("table stats");
        let arena = renderer.material_arena_stats().expect("arena");
        assert_eq!(
            stats.record_groups,
            u64::from(arena.epoch) + 1,
            "one record group per arena buffer: {stats:?} {arena:?}"
        );
        assert_eq!(
            stats.record_groups, first.record_groups,
            "replacing materials built no record group"
        );
        assert!(
            arena.epoch >= 1,
            "300 materials grew the 256-slot arena: {arena:?}"
        );
        assert_eq!(
            renderer.cache_stats().material_uploads - uploads_first,
            5 * PER_FRAME,
            "each replaced key was a new material"
        );
        assert_eq!(
            stats.live, 0,
            "untextured materials sample the neutral views only"
        );
        assert_eq!(
            stats.rebuilds, 1,
            "nothing entered the table after the first build"
        );
        assert_eq!(stats.overflows, 0);
        let opaque = renderer.draw_counts().opaque;
        assert!(opaque.draws > 0, "{opaque:?}");
        assert_eq!(
            opaque.material_batches, 1,
            "one material group for 300 materials: {opaque:?}"
        );
    }
}
