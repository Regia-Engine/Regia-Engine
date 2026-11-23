// SPDX-License-Identifier: MPL-2.0
//! Bindless materials, step 1 (ADR-316, `specs/27` § bindless): what the device offers for
//! bindless material binding ([`BindlessCaps`]), and the one storage buffer that holds every
//! standard material's constants (`MaterialArena`).
//!
//! The capability probe is a pure function of the adapter's features and limits, so it is
//! tested on synthetic limit sets (Mali-like, Adreno-like, desktop Vulkan, DX12-like,
//! lavapipe-like) and decided once per device. The device logs its tier at creation and the
//! agent reports it (`render.get_capabilities`).
//!
//! On tier [`BindlessTier::MaterialSsbo`] and up, a standard material's 352-byte constant block
//! lives in a slot of the arena instead of a uniform buffer of its own. Slots come from a free
//! list with generation keys (the allocator pattern of the mesh arenas, ADR-218), so creating
//! and dropping materials creates no buffer once the arena has grown to the live count. A freed
//! slot is reused only after [`SLOT_RECYCLE_FRAMES`] frames: a refill written before the submit
//! of a frame that already recorded the old material's draws would change that frame. The
//! material's bind group binds its slot (offset `slot × stride`), and the shaders read the
//! block through the same `material` name under the `MATERIAL_SSBO` define
//! ([`MATERIAL_SSBO_DEF`]): the uniform and the storage declaration of one struct read the same
//! bytes, so every pixel is unchanged. On tier [`BindlessTier::None`] the per-material uniform
//! path of earlier releases stays.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use naga_oil::compose::ShaderDefValue;

/// The shader define that switches the material constants binding (group 2, binding 0) from a
/// uniform buffer to a read-only storage buffer in `lab_forward.wgsl`, `lab_shadow.wgsl`,
/// `translucent_shadow.wgsl` and `velocity.wgsl`.
pub const MATERIAL_SSBO_DEF: &str = "MATERIAL_SSBO";

/// Storage buffers per shader stage the storage-buffer material path needs: the visibility
/// resolve's 11 (the most of any pass that binds the material group) plus the material
/// constants. The min-spec mobile profile and the WebGPU default (8) stay on the uniform path.
pub const MATERIAL_SSBO_MIN_STORAGE_BUFFERS: u32 = crate::visbuffer::RESOLVE_STORAGE_BUFFERS + 1;

/// Smallest `max_storage_buffer_binding_size` the storage-buffer path accepts: room for 2048
/// material slots at the widest 512-byte stride, so step 2 can bind the whole arena as one array
/// (the WebGPU floor is 128 MiB, so in practice only a deliberately clamped device is below it).
pub const MATERIAL_SSBO_MIN_BINDING_BYTES: u64 = 1 << 20;

/// Smallest `max_binding_array_elements_per_shader_stage` that names [`BindlessTier::TextureArrays`]:
/// a scene's whole material texture set in one array (step 2).
pub const TEXTURE_ARRAY_MIN_ELEMENTS: u32 = 4096;

/// Material slots of a new arena; it doubles when full.
pub const MATERIAL_ARENA_INITIAL_SLOTS: u32 = 256;

/// Frames a freed slot waits before it is handed out again.
pub const SLOT_RECYCLE_FRAMES: u64 = 2;

/// How far bindless material binding can go on a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BindlessTier {
    /// Per-material uniform buffers and bind groups (min-spec mobile, a low storage-buffer
    /// budget): the path of earlier releases.
    None,
    /// Material constants in one storage buffer; textures stay per material.
    MaterialSsbo,
    /// [`Self::MaterialSsbo`] plus non-uniformly indexed texture binding arrays. The texture
    /// table binds every element, so partially bound arrays are not required (ADR-863); the
    /// table runs on this tier when the device asks for it (`crate::texture_table`).
    TextureArrays,
}

impl BindlessTier {
    /// Stable lowercase name (`none`, `material_ssbo`, `texture_arrays`), as the agent reports it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::MaterialSsbo => "material_ssbo",
            Self::TextureArrays => "texture_arrays",
        }
    }

    /// Whether material constants live in the storage buffer on this tier.
    #[must_use]
    pub fn material_ssbo(self) -> bool {
        self >= Self::MaterialSsbo
    }
}

impl std::fmt::Display for BindlessTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an adapter offers for bindless material binding, and the [`BindlessTier`] it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BindlessCaps {
    /// Backend of the adapter.
    pub backend: wgpu::Backend,
    /// `TEXTURE_BINDING_ARRAY`: arrays of textures in one binding.
    pub binding_arrays: bool,
    /// `SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING`: a per-pixel index into them.
    pub non_uniform_indexing: bool,
    /// `PARTIALLY_BOUND_BINDING_ARRAY`: unused array elements may stay unbound.
    pub partially_bound: bool,
    /// `max_binding_array_elements_per_shader_stage`.
    pub max_binding_array_elements: u32,
    /// `max_storage_buffers_per_shader_stage` (the fragment stage is the one that runs out).
    pub storage_buffers_per_stage: u32,
    /// `max_storage_buffer_binding_size`, bytes.
    pub max_storage_buffer_binding_size: u64,
    /// `min_storage_buffer_offset_alignment`, bytes (the arena's slot stride is a multiple).
    pub storage_offset_alignment: u32,
    /// The tier these name.
    pub tier: BindlessTier,
}

impl BindlessCaps {
    /// Reads the bindless facts from an adapter's (or device's) `features` and `limits` and
    /// names the tier. Pure: the same inputs always give the same tier.
    #[must_use]
    pub fn from_adapter(features: wgpu::Features, limits: &wgpu::Limits, backend: wgpu::Backend) -> Self {
        let binding_arrays = features.contains(wgpu::Features::TEXTURE_BINDING_ARRAY);
        let non_uniform_indexing =
            features.contains(wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING);
        let partially_bound = features.contains(wgpu::Features::PARTIALLY_BOUND_BINDING_ARRAY);
        // GL has no non-uniform indexing in wgpu and WebGPU has no binding arrays.
        let arrays_backend = !matches!(backend, wgpu::Backend::Gl | wgpu::Backend::BrowserWebGpu);
        let tier = if !material_ssbo_limits(limits) {
            BindlessTier::None
        } else if arrays_backend
            && binding_arrays
            && non_uniform_indexing
            && limits.max_binding_array_elements_per_shader_stage >= TEXTURE_ARRAY_MIN_ELEMENTS
        {
            BindlessTier::TextureArrays
        } else {
            BindlessTier::MaterialSsbo
        };
        Self {
            backend,
            binding_arrays,
            non_uniform_indexing,
            partially_bound,
            max_binding_array_elements: limits.max_binding_array_elements_per_shader_stage,
            storage_buffers_per_stage: limits.max_storage_buffers_per_shader_stage,
            max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
            storage_offset_alignment: limits.min_storage_buffer_offset_alignment,
            tier,
        }
    }

    /// [`Self::from_adapter`] on a device's own features, limits and backend. The renderer
    /// requests the binding-array features only at bindless level 3 (`crate::texture_table`), so
    /// a device created below it names at most [`BindlessTier::MaterialSsbo`]; the adapter's
    /// features show what level 3 could request.
    #[must_use]
    pub fn from_device(device: &wgpu::Device) -> Self {
        Self::from_adapter(device.features(), &device.limits(), device.adapter_info().backend)
    }
}

/// Whether `limits` carry the storage-buffer material path ([`BindlessTier::MaterialSsbo`]).
fn material_ssbo_limits(limits: &wgpu::Limits) -> bool {
    limits.max_storage_buffers_per_shader_stage >= MATERIAL_SSBO_MIN_STORAGE_BUFFERS
        && limits.max_storage_buffer_binding_size >= MATERIAL_SSBO_MIN_BINDING_BYTES
}

/// Whether the storage-buffer material path may run on this platform. Android keeps the
/// per-material uniform buffers on every tier for now: tile-based mobile GPUs can serve uniform
/// buffers from a constant cache that storage loads do not use, so the switch waits for a
/// measurement on the owner's Android devices (ADR-316).
fn material_ssbo_platform(android: bool) -> bool {
    !android
}

/// Whether this device's renderer keeps material constants in the storage arena: decided from
/// the platform and the device's limits only, so every pipeline built on one device agrees with
/// its material layout.
#[must_use]
pub fn material_ssbo(device: &wgpu::Device) -> bool {
    material_ssbo_platform(cfg!(target_os = "android")) && material_ssbo_limits(&device.limits())
}

/// The define a module that binds a material's own textures (a graph material's group) adds
/// so `add_material_defs` gives it no texture table defines, though it is not a graph surface:
/// the translucent casters' graph-material variant on the table path (ADR-863). No shader
/// reads it.
pub(crate) const PER_MATERIAL_TEXTURES_DEF: &str = "PER_MATERIAL_TEXTURES";

/// Adds [`MATERIAL_SSBO_DEF`] to `defs` when `device` uses the storage-buffer material path.
/// Every module that declares the material group is composed through this, so its binding
/// matches the material layout of the device. A standard-material module (no `MATERIAL_GRAPH`,
/// `MATERIAL_GRAPH_VERTEX` or `PER_MATERIAL_TEXTURES_DEF` in `defs`) also gets the texture table
/// defines on a device that draws through the table (`crate::texture_table`, ADR-863); graph
/// modules keep their own texture bindings.
pub(crate) fn add_material_defs(device: &wgpu::Device, defs: &mut HashMap<String, ShaderDefValue>) {
    // Every forward module is composed through here: the driver workarounds ride along (ADR-1749).
    crate::shader_workarounds::add_workaround_defs(defs);
    // The frame's blue-noise binding (ADR-2652), on devices whose view layout has it.
    crate::blue_noise::add_forward_defs(device, defs);
    // The forward decals' binding (ADR-3255), on devices whose view layout has it.
    crate::forward_decals::add_forward_defs(device, defs);
    // The process-wide BRDF energy model (ADR-2632); nothing under the default.
    crate::brdf_energy::add_process_def(defs);
    if material_ssbo(device) {
        defs.insert(MATERIAL_SSBO_DEF.to_string(), ShaderDefValue::Bool(true));
        if !keeps_material_textures(defs) {
            crate::texture_table::add_table_defs(device, defs);
        }
    }
}

/// Whether `defs` compose a module that binds a material's own textures: a material-graph
/// module (the graph's surface or vertex host), or one marked with `PER_MATERIAL_TEXTURES_DEF`.
fn keeps_material_textures(defs: &HashMap<String, ShaderDefValue>) -> bool {
    defs.contains_key("MATERIAL_GRAPH")
        || defs.contains_key("MATERIAL_GRAPH_VERTEX")
        || defs.contains_key(PER_MATERIAL_TEXTURES_DEF)
}

/// The material constants entry (group 2, binding 0) of the material layout: a uniform buffer
/// on [`BindlessTier::None`], a read-only storage buffer otherwise.
#[must_use]
pub(crate) fn material_constants_entry(ssbo: bool) -> wgpu::BindGroupLayoutEntry {
    let ty = if ssbo {
        wgpu::BufferBindingType::Storage { read_only: true }
    } else {
        wgpu::BufferBindingType::Uniform
    };
    wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// The material record entry (group 2, binding 0) on the texture table path (ADR-863): one
/// read-only storage binding of `record` bytes with a dynamic offset, so a single bind group
/// over the whole arena serves every standard material and a draw selects its record by
/// offset (`slot × stride`). The table holds the textures, so the group has no other entry
/// (wgpu refuses a dynamic offset in a layout that also holds a binding array; the table is
/// its own group).
#[must_use]
pub(crate) fn material_record_entry(record: u64) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: true,
            min_binding_size: std::num::NonZeroU64::new(record),
        },
        count: None,
    }
}

/// A material's slot in the arena: its index and the generation it was handed out under. A
/// freed slot's key is stale for good; the index may be handed out again under a new generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaterialSlot {
    index: u32,
    generation: u32,
}

impl MaterialSlot {
    /// Index of the slot in the arena (the record at `index × stride`).
    #[must_use]
    pub fn index(self) -> u32 {
        self.index
    }

    /// Generation the slot was allocated under.
    #[must_use]
    pub fn generation(self) -> u32 {
        self.generation
    }
}

/// Free-list slot allocator with generation keys and a recycle delay (no GPU state; the arena
/// holds one).
#[derive(Debug, Default)]
pub(crate) struct SlotAllocator {
    /// Current generation of each index.
    generations: Vec<u32>,
    /// Whether each index is handed out.
    occupied: Vec<bool>,
    /// Indices free to hand out, last-reclaimed first.
    free: Vec<u32>,
    /// `(index, frame it was freed)`, waiting [`SLOT_RECYCLE_FRAMES`].
    retired: Vec<(u32, u64)>,
    live: usize,
}

impl SlotAllocator {
    /// A slot: a recycled index when there is one, else the next new index.
    pub(crate) fn allocate(&mut self) -> MaterialSlot {
        self.live += 1;
        if let Some(index) = self.free.pop() {
            let i = index as usize;
            self.occupied[i] = true;
            return MaterialSlot {
                index,
                generation: self.generations[i],
            };
        }
        let index = u32::try_from(self.generations.len()).unwrap_or(u32::MAX);
        self.generations.push(0);
        self.occupied.push(true);
        MaterialSlot { index, generation: 0 }
    }

    /// Whether `slot` is handed out under its generation (a freed or forged key is not).
    #[must_use]
    pub(crate) fn is_live(&self, slot: MaterialSlot) -> bool {
        let i = slot.index as usize;
        self.occupied.get(i).copied().unwrap_or(false) && self.generations[i] == slot.generation
    }

    /// Retires `slot` (freed in `frame`) and its generation; the index is reused once
    /// [`Self::reclaim`] passes [`SLOT_RECYCLE_FRAMES`] later. `false` (nothing changes) for a
    /// stale key.
    pub(crate) fn free(&mut self, slot: MaterialSlot, frame: u64) -> bool {
        if !self.is_live(slot) {
            return false;
        }
        let i = slot.index as usize;
        self.occupied[i] = false;
        self.generations[i] = self.generations[i].wrapping_add(1);
        self.retired.push((slot.index, frame));
        self.live -= 1;
        true
    }

    /// Makes the slots retired at least [`SLOT_RECYCLE_FRAMES`] before `frame` reusable.
    pub(crate) fn reclaim(&mut self, frame: u64) {
        let free = &mut self.free;
        self.retired.retain(|&(index, freed)| {
            let ready = freed.saturating_add(SLOT_RECYCLE_FRAMES) <= frame;
            if ready {
                free.push(index);
            }
            !ready
        });
    }

    /// Slots handed out now.
    #[must_use]
    pub(crate) fn live(&self) -> usize {
        self.live
    }

    /// Indices ever handed out: the capacity the arena needs.
    #[must_use]
    pub(crate) fn high_water(&self) -> u32 {
        u32::try_from(self.generations.len()).unwrap_or(u32::MAX)
    }
}

/// Slots of an arena that must hold `needed`: [`MATERIAL_ARENA_INITIAL_SLOTS`] doubled until it
/// fits.
#[must_use]
pub(crate) fn arena_capacity_for(current: u32, needed: u32) -> u32 {
    let mut capacity = current.max(MATERIAL_ARENA_INITIAL_SLOTS);
    while capacity < needed {
        capacity = capacity.saturating_mul(2);
    }
    capacity
}

/// `record` rounded up to the storage offset `alignment`: the distance between two slots.
#[must_use]
pub(crate) fn slot_stride(record: u64, alignment: u32) -> u64 {
    record.next_multiple_of(u64::from(alignment.max(4)))
}

/// What the material constants storage buffer holds now
/// ([`crate::FrameRenderer::material_arena_stats`], `render.get_frame_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaterialArenaStats {
    /// Materials holding a slot.
    pub live: u64,
    /// Slots the buffer has room for.
    pub capacity: u32,
    /// Bytes between two slots (the 352-byte record at the device's storage offset alignment).
    pub stride: u64,
    /// Buffers the arena created (the first one included); flat after warm-up.
    pub buffer_creates: u64,
    /// Times the buffer was replaced by a larger one (each rebuilds the material bind groups).
    pub epoch: u32,
}

/// Source of process-unique arena identities.
static NEXT_ARENA_ID: AtomicU64 = AtomicU64::new(1);

/// Every standard material's constants in one storage buffer (tier [`BindlessTier::MaterialSsbo`]).
///
/// Slots are allocated from a `SlotAllocator`; the buffer doubles when the live slots outgrow
/// it (a CPU mirror refills the new buffer, and [`Self::epoch`] tells bind groups built on the old
/// buffer to rebuild). Freeing a slot never shrinks the buffer, so a scene that loads and drops
/// materials creates no buffer after it reached its peak.
pub(crate) struct MaterialArena {
    id: u64,
    slots: SlotAllocator,
    /// Bytes of one record (the shader's `LabMaterial` without a graph block).
    record: u64,
    /// Bytes between two slots (`record` at the storage offset alignment).
    stride: u64,
    capacity: u32,
    buffer: wgpu::Buffer,
    /// Every slot's bytes, for refilling a grown buffer.
    mirror: Vec<u8>,
    /// Bumped each time the buffer is replaced.
    epoch: u32,
    /// Buffers this arena created (the first included).
    buffer_creates: u64,
}

impl MaterialArena {
    /// An empty arena of [`MATERIAL_ARENA_INITIAL_SLOTS`] records of `record` bytes on `device`.
    pub(crate) fn new(device: &wgpu::Device, record: u64) -> Self {
        let stride = slot_stride(record, device.limits().min_storage_buffer_offset_alignment);
        let capacity = MATERIAL_ARENA_INITIAL_SLOTS;
        let buffer = arena_buffer(device, stride, capacity);
        Self {
            id: NEXT_ARENA_ID.fetch_add(1, Ordering::Relaxed),
            slots: SlotAllocator::default(),
            record,
            stride,
            capacity,
            buffer,
            mirror: vec![0; mirror_len(stride, capacity)],
            epoch: 0,
            buffer_creates: 1,
        }
    }

    /// Process-unique identity (a material entry built against another arena rebuilds).
    #[must_use]
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Bumped whenever the buffer was replaced by a larger one.
    #[must_use]
    pub(crate) fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Live slots, capacity, stride and buffer creations.
    #[must_use]
    pub(crate) fn stats(&self) -> MaterialArenaStats {
        MaterialArenaStats {
            live: self.slots.live() as u64,
            capacity: self.capacity,
            stride: self.stride,
            buffer_creates: self.buffer_creates,
            epoch: self.epoch,
        }
    }

    /// Makes slots freed at least [`SLOT_RECYCLE_FRAMES`] before `frame` reusable (once per
    /// frame, before any material is created).
    pub(crate) fn reclaim(&mut self, frame: u64) {
        self.slots.reclaim(frame);
    }

    /// A slot holding `bytes` (at most one record). Grows the buffer when every slot is taken.
    pub(crate) fn allocate(
        &mut self,
        device: &wgpu::Device,
        queue: &crate::device::TrackedQueue,
        bytes: &[u8],
    ) -> MaterialSlot {
        let slot = self.slots.allocate();
        let needed = self.slots.high_water();
        if needed > self.capacity {
            let capacity = arena_capacity_for(self.capacity, needed);
            let _span = tracing::info_span!("material_arena_grow", capacity).entered();
            self.mirror.resize(mirror_len(self.stride, capacity), 0);
            self.buffer = arena_buffer(device, self.stride, capacity);
            self.capacity = capacity;
            self.epoch = self.epoch.wrapping_add(1);
            self.buffer_creates += 1;
            self.write_mirror(slot, bytes);
            // The live records move to the new buffer in one copy; old bind groups keep the old
            // buffer alive until they are rebuilt.
            queue.write_buffer(&self.buffer, 0, &self.mirror);
            return slot;
        }
        self.write(queue, slot, bytes);
        slot
    }

    /// Rewrites `slot`'s record with `bytes`. `false` (nothing written) for a stale key.
    pub(crate) fn write(
        &mut self,
        queue: &crate::device::TrackedQueue,
        slot: MaterialSlot,
        bytes: &[u8],
    ) -> bool {
        if !self.slots.is_live(slot) {
            return false;
        }
        let offset = self.offset(slot);
        self.write_mirror(slot, bytes);
        let len = bytes.len().min(self.record as usize);
        queue.write_buffer(&self.buffer, offset, &bytes[..len]);
        true
    }

    /// Rewrites `bytes` at `offset` inside `slot`'s record (the texture table indices after the
    /// constants, ADR-863). `false` (nothing written) for a stale key or a range past the record.
    pub(crate) fn write_at(
        &mut self,
        queue: &crate::device::TrackedQueue,
        slot: MaterialSlot,
        offset: u64,
        bytes: &[u8],
    ) -> bool {
        if !self.slots.is_live(slot) || offset + bytes.len() as u64 > self.record {
            return false;
        }
        let start = self.offset(slot) + offset;
        let first = start as usize;
        if let Some(dst) = self.mirror.get_mut(first..first + bytes.len()) {
            dst.copy_from_slice(bytes);
        }
        queue.write_buffer(&self.buffer, start, bytes);
        true
    }

    /// Retires `slot` (freed in `frame`). `false` for a stale key.
    pub(crate) fn free(&mut self, slot: MaterialSlot, frame: u64) -> bool {
        self.slots.free(slot, frame)
    }

    /// The buffer range of `slot`'s record (`None` for a stale key): what the material's bind
    /// group binds.
    #[must_use]
    pub(crate) fn binding(&self, slot: MaterialSlot) -> Option<wgpu::BufferBinding<'_>> {
        self.slots.is_live(slot).then(|| wgpu::BufferBinding {
            buffer: &self.buffer,
            offset: self.offset(slot),
            size: std::num::NonZeroU64::new(self.record),
        })
    }

    /// The first record of the buffer (offset 0, one record long): what the shared record group
    /// of the texture table path binds (`material_record_entry`); draws pick their record with
    /// [`Self::dynamic_offset`].
    #[must_use]
    pub(crate) fn record_binding(&self) -> wgpu::BufferBinding<'_> {
        wgpu::BufferBinding {
            buffer: &self.buffer,
            offset: 0,
            size: std::num::NonZeroU64::new(self.record),
        }
    }

    /// The whole buffer: what the single-pass visibility resolve binds (ADR-2071), each pixel
    /// picking its record by `slot × stride`.
    #[must_use]
    pub(crate) fn whole_binding(&self) -> wgpu::BufferBinding<'_> {
        self.buffer.as_entire_buffer_binding()
    }

    /// The dynamic offset of `slot`'s record (`slot × stride`, a multiple of the storage offset
    /// alignment). `None` for a stale key, or an offset past `u32` (an arena over 4 GiB).
    #[must_use]
    pub(crate) fn dynamic_offset(&self, slot: MaterialSlot) -> Option<u32> {
        if !self.slots.is_live(slot) {
            return None;
        }
        u32::try_from(self.offset(slot)).ok()
    }

    fn offset(&self, slot: MaterialSlot) -> u64 {
        u64::from(slot.index) * self.stride
    }

    fn write_mirror(&mut self, slot: MaterialSlot, bytes: &[u8]) {
        let start = self.offset(slot) as usize;
        let len = bytes.len().min(self.record as usize);
        if let Some(dst) = self.mirror.get_mut(start..start + len) {
            dst.copy_from_slice(&bytes[..len]);
        }
    }
}

fn mirror_len(stride: u64, capacity: u32) -> usize {
    usize::try_from(stride * u64::from(capacity)).unwrap_or(usize::MAX)
}

fn arena_buffer(device: &wgpu::Device, stride: u64, capacity: u32) -> wgpu::Buffer {
    crate::device::TrackedDevice::observe(device).create_buffer(&wgpu::BufferDescriptor {
        label: Some("material arena"),
        size: stride * u64::from(capacity),
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Desktop Vulkan (RTX-class): everything, so binding arrays name the top tier.
    fn desktop_vulkan() -> (wgpu::Features, wgpu::Limits) {
        let features = wgpu::Features::TEXTURE_BINDING_ARRAY
            | wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING
            | wgpu::Features::PARTIALLY_BOUND_BINDING_ARRAY
            | wgpu::Features::BUFFER_BINDING_ARRAY
            | wgpu::Features::STORAGE_RESOURCE_BINDING_ARRAY;
        let limits = wgpu::Limits {
            max_storage_buffers_per_shader_stage: 1_048_576,
            max_storage_buffer_binding_size: 2_147_483_644,
            max_binding_array_elements_per_shader_stage: 500_000,
            min_storage_buffer_offset_alignment: 16,
            ..wgpu::Limits::default()
        };
        (features, limits)
    }

    /// Mali-like min-spec Android: the WebGPU default of 8 storage buffers per stage, no
    /// descriptor indexing.
    fn mali_like() -> (wgpu::Features, wgpu::Limits) {
        let limits = wgpu::Limits {
            max_storage_buffers_per_shader_stage: 8,
            max_storage_buffer_binding_size: 128 << 20,
            max_binding_array_elements_per_shader_stage: 0,
            min_storage_buffer_offset_alignment: 256,
            ..wgpu::Limits::default()
        };
        (wgpu::Features::empty(), limits)
    }

    /// Adreno-like: a generous storage budget and binding arrays with non-uniform indexing, but
    /// no partially bound arrays.
    fn adreno_like() -> (wgpu::Features, wgpu::Limits) {
        let features = wgpu::Features::TEXTURE_BINDING_ARRAY
            | wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING;
        let limits = wgpu::Limits {
            max_storage_buffers_per_shader_stage: 24,
            max_storage_buffer_binding_size: 512 << 20,
            max_binding_array_elements_per_shader_stage: 65_536,
            min_storage_buffer_offset_alignment: 64,
            ..wgpu::Limits::default()
        };
        (features, limits)
    }

    /// DX12 Resource Binding Tier 2 (no binding arrays through wgpu, `ResourceBindingTier`).
    fn dx12_tier2() -> (wgpu::Features, wgpu::Limits) {
        let limits = wgpu::Limits {
            max_storage_buffers_per_shader_stage: 64,
            max_storage_buffer_binding_size: 128 << 20,
            min_storage_buffer_offset_alignment: 256,
            ..wgpu::Limits::default()
        };
        (wgpu::Features::empty(), limits)
    }

    /// lavapipe-like: large limits, descriptor indexing without partially bound arrays.
    fn lavapipe_like() -> (wgpu::Features, wgpu::Limits) {
        let features = wgpu::Features::TEXTURE_BINDING_ARRAY
            | wgpu::Features::SAMPLED_TEXTURE_AND_STORAGE_BUFFER_ARRAY_NON_UNIFORM_INDEXING;
        let limits = wgpu::Limits {
            max_storage_buffers_per_shader_stage: 1_000_000,
            max_storage_buffer_binding_size: 2_147_483_644,
            max_binding_array_elements_per_shader_stage: 1_000_000,
            min_storage_buffer_offset_alignment: 16,
            ..wgpu::Limits::default()
        };
        (features, limits)
    }

    fn tier(set: (wgpu::Features, wgpu::Limits), backend: wgpu::Backend) -> BindlessTier {
        BindlessCaps::from_adapter(set.0, &set.1, backend).tier
    }

    #[test]
    fn synthetic_limit_sets_name_their_tiers() {
        use wgpu::Backend::{Dx12, Vulkan};
        assert_eq!(tier(mali_like(), Vulkan), BindlessTier::None);
        // Binding arrays without partially bound arrays name the top tier (ADR-863: the table
        // binds every element).
        assert_eq!(tier(adreno_like(), Vulkan), BindlessTier::TextureArrays);
        assert_eq!(tier(desktop_vulkan(), Vulkan), BindlessTier::TextureArrays);
        assert_eq!(tier(dx12_tier2(), Dx12), BindlessTier::MaterialSsbo);
        // DX12 Tier 3: wgpu exposes the binding-array features (`ResourceBindingTier`).
        assert_eq!(tier(desktop_vulkan(), Dx12), BindlessTier::TextureArrays);
        assert_eq!(tier(lavapipe_like(), Vulkan), BindlessTier::TextureArrays);
        // Metal has no partially bound arrays at all (the wgpu 30 feature docs).
        let (features, limits) = desktop_vulkan();
        let metal_like = (features - wgpu::Features::PARTIALLY_BOUND_BINDING_ARRAY, limits);
        assert_eq!(
            tier(metal_like, wgpu::Backend::Metal),
            BindlessTier::TextureArrays
        );
    }

    #[test]
    fn the_probe_reads_every_input() {
        let (features, limits) = adreno_like();
        let caps = BindlessCaps::from_adapter(features, &limits, wgpu::Backend::Vulkan);
        assert!(caps.binding_arrays);
        assert!(caps.non_uniform_indexing);
        assert!(!caps.partially_bound);
        assert_eq!(caps.max_binding_array_elements, 65_536);
        assert_eq!(caps.storage_buffers_per_stage, 24);
        assert_eq!(caps.max_storage_buffer_binding_size, 512 << 20);
        assert_eq!(caps.storage_offset_alignment, 64);
        assert_eq!(caps.backend, wgpu::Backend::Vulkan);
    }

    #[test]
    fn each_threshold_drops_exactly_one_tier() {
        let (features, mut limits) = desktop_vulkan();
        limits.max_binding_array_elements_per_shader_stage = TEXTURE_ARRAY_MIN_ELEMENTS - 1;
        assert_eq!(
            BindlessCaps::from_adapter(features, &limits, wgpu::Backend::Vulkan).tier,
            BindlessTier::MaterialSsbo,
            "too few array elements keeps the storage buffer only"
        );
        let (features, mut limits) = desktop_vulkan();
        limits.max_storage_buffers_per_shader_stage = MATERIAL_SSBO_MIN_STORAGE_BUFFERS - 1;
        assert_eq!(
            BindlessCaps::from_adapter(features, &limits, wgpu::Backend::Vulkan).tier,
            BindlessTier::None,
            "the visibility resolve plus the material buffer must fit one stage"
        );
        let (features, mut limits) = desktop_vulkan();
        limits.max_storage_buffers_per_shader_stage = MATERIAL_SSBO_MIN_STORAGE_BUFFERS;
        limits.max_storage_buffer_binding_size = MATERIAL_SSBO_MIN_BINDING_BYTES - 4;
        assert_eq!(
            BindlessCaps::from_adapter(features, &limits, wgpu::Backend::Vulkan).tier,
            BindlessTier::None
        );
        let (features, limits) = desktop_vulkan();
        assert_eq!(
            BindlessCaps::from_adapter(features, &limits, wgpu::Backend::Gl).tier,
            BindlessTier::MaterialSsbo,
            "GL never names texture arrays"
        );
    }

    #[test]
    fn the_mobile_minspec_profile_keeps_the_uniform_path() {
        let profile = crate::minspec::MinspecProfile::Mobile;
        let (features, limits) = desktop_vulkan();
        let clamped = profile.clamp_limits(&limits);
        let caps =
            BindlessCaps::from_adapter(features & profile.feature_mask(), &clamped, wgpu::Backend::Vulkan);
        assert_eq!(caps.tier, BindlessTier::None);
    }

    #[test]
    fn android_keeps_the_uniform_path_until_measured() {
        assert!(!material_ssbo_platform(true));
        assert!(material_ssbo_platform(false));
    }

    #[test]
    fn tier_names_are_stable_protocol_strings() {
        assert_eq!(BindlessTier::None.as_str(), "none");
        assert_eq!(BindlessTier::MaterialSsbo.to_string(), "material_ssbo");
        assert_eq!(BindlessTier::TextureArrays.as_str(), "texture_arrays");
        assert!(!BindlessTier::None.material_ssbo());
        assert!(BindlessTier::MaterialSsbo.material_ssbo());
        assert!(BindlessTier::TextureArrays.material_ssbo());
    }

    #[test]
    fn slot_strides_follow_the_storage_offset_alignment() {
        assert_eq!(slot_stride(352, 256), 512);
        assert_eq!(slot_stride(352, 64), 384);
        assert_eq!(slot_stride(352, 16), 352);
        assert_eq!(slot_stride(352, 0), 352);
        assert_eq!(arena_capacity_for(0, 1), MATERIAL_ARENA_INITIAL_SLOTS);
        assert_eq!(arena_capacity_for(256, 257), 512);
        assert_eq!(arena_capacity_for(256, 10_000), 16_384);
    }

    /// Churn on the allocator alone: 10 000 materials, freed and allocated again, reuse every
    /// index once the recycle delay passed (no growth after the first round), and every freed
    /// key stays stale.
    #[test]
    fn churn_reuses_slots_and_rejects_stale_keys() {
        let mut slots = SlotAllocator::default();
        let mut frame = 0u64;
        let first: Vec<MaterialSlot> = (0..10_000).map(|_| slots.allocate()).collect();
        assert_eq!(slots.high_water(), 10_000);
        for slot in &first {
            assert!(slots.free(*slot, frame));
        }
        assert_eq!(slots.live(), 0);
        for _round in 0..3 {
            frame += SLOT_RECYCLE_FRAMES;
            slots.reclaim(frame);
            let again: Vec<MaterialSlot> = (0..10_000).map(|_| slots.allocate()).collect();
            assert_eq!(slots.high_water(), 10_000, "warm churn needs no new index");
            for slot in &again {
                assert!(slots.free(*slot, frame));
            }
        }
        for slot in &first {
            assert!(!slots.is_live(*slot), "a freed key is stale");
            assert!(!slots.free(*slot, frame), "a stale key frees nothing");
        }
        frame += SLOT_RECYCLE_FRAMES;
        slots.reclaim(frame);
        let reused = slots.allocate();
        let old = first
            .iter()
            .find(|slot| slot.index() == reused.index())
            .copied()
            .unwrap_or(reused);
        assert_ne!(
            old.generation(),
            reused.generation(),
            "a reused index has a new generation"
        );
        assert!(slots.is_live(reused));
        assert!(!slots.is_live(MaterialSlot {
            index: 99_999,
            generation: 0,
        }));
    }

    /// A slot freed in a frame is not handed out again in that frame or the next: a refill
    /// would land before the submit of draws that still read the old material.
    #[test]
    fn freed_slots_wait_before_reuse() {
        let mut slots = SlotAllocator::default();
        let a = slots.allocate();
        assert!(slots.free(a, 10));
        slots.reclaim(10);
        assert_eq!(slots.allocate().index(), 1, "same frame: a new index");
        slots.reclaim(11);
        assert_eq!(slots.allocate().index(), 2, "next frame: still waiting");
        slots.reclaim(10 + SLOT_RECYCLE_FRAMES);
        assert_eq!(slots.allocate().index(), a.index(), "after the delay: reused");
    }

    /// The GPU arena under churn: after the first 10 000 materials grew it, allocating and
    /// freeing 10 000 more (three rounds, frames apart) creates no buffer, and stale keys write
    /// and bind nothing.
    #[test]
    fn arena_churn_creates_no_buffer_after_warm_up() {
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let d = &device.device;
        let record = [7u8; 352];
        let mut arena = MaterialArena::new(d, 352);
        let mut frame = 0u64;
        let warm: Vec<MaterialSlot> = (0..10_000).map(|_| arena.allocate(d, &queue, &record)).collect();
        let warm_stats = arena.stats();
        assert!(warm_stats.capacity >= 10_000);
        assert_eq!(warm_stats.live, 10_000);
        let buffers_before = crate::frame_stats::counters().buffers;
        for slot in &warm {
            assert!(arena.free(*slot, frame));
        }
        for _round in 0..3 {
            frame += SLOT_RECYCLE_FRAMES;
            arena.reclaim(frame);
            let slots: Vec<MaterialSlot> = (0..10_000).map(|_| arena.allocate(d, &queue, &record)).collect();
            for slot in &slots {
                assert!(arena.free(*slot, frame));
            }
        }
        let after = arena.stats();
        assert_eq!(
            after.buffer_creates, warm_stats.buffer_creates,
            "no arena growth after warm-up"
        );
        assert_eq!(
            after.epoch, warm_stats.epoch,
            "no bind group rebuild after warm-up"
        );
        assert_eq!(after.capacity, warm_stats.capacity);
        assert_eq!(
            crate::frame_stats::counters().buffers,
            buffers_before,
            "material churn creates no buffer"
        );
        for slot in &warm {
            assert!(!arena.write(&queue, *slot, &record), "stale keys write nothing");
            assert!(arena.binding(*slot).is_none(), "stale keys bind nothing");
        }
        assert_eq!(after.live, 0);
    }

    fn compose(source: &str, file: &str, defs: &[&str]) -> naga::Module {
        let map: HashMap<String, ShaderDefValue> = defs
            .iter()
            .map(|name| ((*name).to_string(), ShaderDefValue::Bool(true)))
            .collect();
        crate::shader_lib::compose_source(source, file, map)
            .unwrap_or_else(|err| panic!("{file} {defs:?}: {err}"))
    }

    fn validate(module: &naga::Module, what: &str) {
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(module)
        .unwrap_or_else(|err| panic!("{what}: {err:?}"));
    }

    /// The material binding's address space (group 2, binding 0).
    fn material_space(module: &naga::Module) -> Option<naga::AddressSpace> {
        module
            .global_variables
            .iter()
            .find(|(_, var)| {
                var.binding
                    .as_ref()
                    .is_some_and(|b| b.group == 2 && b.binding == 0)
            })
            .map(|(_, var)| var.space)
    }

    fn storage_read() -> naga::AddressSpace {
        naga::AddressSpace::Storage {
            access: naga::StorageAccess::LOAD,
        }
    }

    /// The forward shader (CPU, instanced, glass and mask variants), the visibility resolve
    /// (which composes it) and the translucent caster shader parse and validate under both
    /// paths, and the define flips exactly the material binding's address space.
    #[test]
    fn material_accessor_compiles_under_both_paths() {
        let forward = include_str!("../shaders/lab_forward.wgsl");
        let caster = include_str!("../shaders/translucent_shadow.wgsl");
        let resolve = crate::visbuffer::gpu::resolve_source();
        let cases: [(&str, &str, &[&str]); 9] = [
            (forward, "lab_forward.wgsl", &["ALPHA_TEST"]),
            (
                forward,
                "lab_forward.wgsl",
                &["GPU_DRIVEN", "GPU_INSTANCED", "ALPHA_TEST"],
            ),
            (forward, "lab_forward.wgsl", &["GLASS", "ALPHA_TEST"]),
            (forward, "lab_forward.wgsl", &["MASK_DECALS", "ALPHA_TEST"]),
            (forward, "lab_forward.wgsl", &["THIN_FILM", "ALPHA_TEST"]),
            (resolve, "vis_resolve.wgsl", &["VISBUFFER"]),
            (caster, "translucent_shadow.wgsl", &[]),
            (caster, "translucent_shadow.wgsl", &["GLASS"]),
            (
                include_str!("../shaders/lab_shadow.wgsl"),
                "lab_shadow.wgsl",
                &["GPU_DRIVEN", "ALPHA_TEST"],
            ),
        ];
        for (source, file, defs) in cases {
            let uniform = compose(source, file, defs);
            validate(&uniform, &format!("{file} {defs:?}"));
            assert_eq!(
                material_space(&uniform),
                Some(naga::AddressSpace::Uniform),
                "{file} {defs:?}"
            );
            let mut with_ssbo: Vec<&str> = defs.to_vec();
            with_ssbo.push(MATERIAL_SSBO_DEF);
            let storage = compose(source, file, &with_ssbo);
            validate(&storage, &format!("{file} {with_ssbo:?}"));
            assert_eq!(
                material_space(&storage),
                Some(storage_read()),
                "{file} {with_ssbo:?}"
            );
        }
    }

    /// A vertex-stage material graph's shadow and velocity hosts declare the material group
    /// too; with the sample graph they validate under both paths.
    #[test]
    fn graph_vertex_hosts_compile_under_both_paths() {
        let code = crate::surface_graph::GraphCode {
            surface: crate::surface_graph::SAMPLE_SURFACE.into(),
            vertex: Some(crate::surface_graph::SAMPLE_VERTEX.into()),
            // The combination `surface_graph`'s own host test validates.
            uv1: true,
        };
        for velocity in [false, true] {
            let Some(source) =
                crate::surface_graph::vertex_host_source(&code, velocity).expect("host source")
            else {
                panic!("a vertex graph has a host (velocity {velocity})");
            };
            for ssbo in [false, true] {
                let extra: &[&str] = if ssbo { &[MATERIAL_SSBO_DEF] } else { &[] };
                let module = crate::shader_lib::compose_source(
                    &source,
                    "graph_host.wgsl",
                    crate::surface_graph::vertex_host_defs(&code, extra),
                )
                .expect("composes");
                validate(&module, &format!("graph host velocity {velocity} ssbo {ssbo}"));
                let expected = if ssbo {
                    storage_read()
                } else {
                    naga::AddressSpace::Uniform
                };
                assert_eq!(material_space(&module), Some(expected));
            }
        }
    }

    /// The table path's record entry (ADR-863): one storage record of the arena's record size at
    /// a dynamic offset, so one group serves every standard material.
    #[test]
    fn the_record_entry_is_one_record_at_a_dynamic_offset() {
        let entry = material_record_entry(368);
        assert_eq!(entry.binding, 0);
        assert_eq!(entry.visibility, wgpu::ShaderStages::VERTEX_FRAGMENT);
        assert!(entry.count.is_none());
        assert!(matches!(
            entry.ty,
            wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: true,
                min_binding_size: Some(size),
            } if size.get() == 368
        ));
    }

    /// Dynamic offsets are `slot × stride` (multiples of the storage offset alignment), the
    /// shared binding is one record at offset 0, and a stale key has no offset.
    #[test]
    fn arena_dynamic_offsets_follow_the_slots() {
        let device = crate::device::test_device();
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        let d = &device.device;
        let record = [3u8; 368];
        let mut arena = MaterialArena::new(d, 368);
        let stride = arena.stats().stride;
        let alignment = u64::from(d.limits().min_storage_buffer_offset_alignment.max(4));
        assert!(
            stride.is_multiple_of(alignment),
            "stride {stride} at alignment {alignment}"
        );
        let slots: Vec<MaterialSlot> = (0..3).map(|_| arena.allocate(d, &queue, &record)).collect();
        for slot in &slots {
            let offset = arena.dynamic_offset(*slot).expect("live slot");
            assert_eq!(u64::from(offset), u64::from(slot.index()) * stride);
            assert!(u64::from(offset).is_multiple_of(alignment));
        }
        let binding = arena.record_binding();
        assert_eq!(binding.offset, 0);
        assert_eq!(binding.size.map(std::num::NonZeroU64::get), Some(368));
        assert!(arena.free(slots[1], 0));
        assert_eq!(arena.dynamic_offset(slots[1]), None, "a stale key has no offset");
        assert!(
            arena.write_at(&queue, slots[0], 352, &[1u8; 16]),
            "the index words fit the record"
        );
        assert!(
            !arena.write_at(&queue, slots[0], 360, &[1u8; 16]),
            "a range past the record is refused"
        );
        assert!(
            !arena.write_at(&queue, slots[1], 352, &[1u8; 16]),
            "a stale key writes nothing"
        );
    }

    /// Graph modules and the per-material-textures caster variant never get the texture table
    /// defines; every other module does on a table device (ADR-863).
    #[test]
    fn modules_with_their_own_textures_skip_the_table() {
        let defs = |names: &[&str]| -> HashMap<String, ShaderDefValue> {
            names
                .iter()
                .map(|name| ((*name).to_string(), ShaderDefValue::Bool(true)))
                .collect()
        };
        assert!(!keeps_material_textures(&defs(&[])));
        assert!(!keeps_material_textures(&defs(&["GLASS", "ALPHA_TEST"])));
        assert!(keeps_material_textures(&defs(&["MATERIAL_GRAPH"])));
        assert!(keeps_material_textures(&defs(&["MATERIAL_GRAPH_VERTEX"])));
        assert!(keeps_material_textures(&defs(&[PER_MATERIAL_TEXTURES_DEF])));
    }

    #[test]
    fn the_layout_entry_follows_the_path() {
        let uniform = material_constants_entry(false);
        let storage = material_constants_entry(true);
        assert!(matches!(
            uniform.ty,
            wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                ..
            }
        ));
        assert!(matches!(
            storage.ty,
            wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                ..
            }
        ));
        assert_eq!(uniform.visibility, wgpu::ShaderStages::VERTEX_FRAGMENT);
        assert_eq!(storage.binding, 0);
    }
}

/// Frame-level checks: the draw counters on a fixed scene (exact counts, no timing) and the
/// storage-buffer path against the uniform path on the same adapter (identical pixels).
#[cfg(test)]
mod frame_tests {
    use crate::{
        ALPHA_BLEND, DirectionalLightUniform, FrameDrawCounts, FrameQuality, FrameRenderer, LabMaterial,
        LabMesh, LabScene, LabTexture, LabVertex, PassDrawCounts, RenderDevice,
    };

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

    fn tinted(r: f32, roughness: f32) -> LabMaterial {
        LabMaterial {
            base_color: [r, 0.5, 0.25, 1.0],
            roughness,
            ..LabMaterial::default()
        }
    }

    fn pass(draws: u64, pipelines: u64, groups: [u64; 4], batches: u64) -> PassDrawCounts {
        PassDrawCounts {
            draws,
            pipeline_switches: pipelines,
            bind_group_switches: groups,
            material_batches: batches,
        }
    }

    /// Four opaque quads, two per material, in scene order, no shadows, CPU path (`LAB`): the
    /// prepass and the opaque pass each bind the view once and one pipeline, every quad's object
    /// group, and each material group once (two runs of two draws). Nothing else is drawn.
    #[test]
    fn draw_counters_are_exact_on_a_fixed_scene() {
        let device = crate::device::test_device();
        let lab = scene(
            vec![tinted(0.8, 0.4), tinted(0.2, 0.7)],
            vec![
                quad(-1.2, 0.0, 0),
                quad(-0.4, 0.0, 0),
                quad(0.4, 0.0, 1),
                quad(1.2, 0.0, 1),
            ],
            false,
        );
        let frame = lab.as_frame_scene_with_quality(FrameQuality::LAB);
        let mut renderer = FrameRenderer::new();
        assert_eq!(
            renderer.draw_counts(),
            FrameDrawCounts::default(),
            "zero before the first frame"
        );
        let mut seen = Vec::new();
        for _ in 0..3 {
            renderer.render_offscreen(&device, &frame).expect("frame");
            seen.push(renderer.draw_counts());
        }
        assert_eq!(seen[1], seen[2], "steady frames count the same calls");
        let counts = seen[2];
        // On the texture table path (bindless level 3, ADR-863) both materials bind the one
        // shared record group at their own offsets: two group-2 switches, one material batch.
        let table = crate::texture_table::device_support(&device.device)
            .capacity()
            .is_some();
        let batches = if table { 1 } else { 2 };
        let expected = pass(4, 1, [1, 4, 2, 0], batches);
        assert_eq!(counts.depth_prepass, expected, "{counts:?}");
        assert_eq!(counts.opaque, expected, "{counts:?}");
        assert_eq!(counts.translucent, PassDrawCounts::default());
        assert_eq!(
            counts.shadow,
            PassDrawCounts::default(),
            "no shadow jobs without shadows"
        );
        assert_eq!(counts.visbuffer_resolve, PassDrawCounts::default());
        assert_eq!(counts.total(), pass(8, 2, [2, 8, 4, 0], 2 * batches));
        // The arena holds the two standard materials on the storage path.
        if crate::bindless::material_ssbo(&device.device) {
            let arena = renderer
                .material_arena_stats()
                .expect("storage path has an arena");
            assert_eq!(arena.live, 2);
            assert_eq!(arena.buffer_creates, 1);
        } else {
            assert!(renderer.material_arena_stats().is_none());
        }
    }

    /// A device on `adapter`'s default pick with `max_storage_buffers_per_shader_stage` capped
    /// (`None`: the adapter's own limits). Both test devices are built this way, so they differ
    /// only in that limit.
    fn device_with_storage_cap(cap: Option<u32>) -> Option<RenderDevice> {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = crate::device::instance_backends();
        descriptor.flags = crate::device::instance_flags();
        let instance = crate::device::create_instance(descriptor);
        let adapter = crate::gpu_select::select(&instance)
            .0
            .or_else(|| crate::gpu_select::default_adapter(&instance, None))?;
        let mut limits = adapter.limits();
        if let Some(cap) = cap {
            limits.max_storage_buffers_per_shader_stage =
                limits.max_storage_buffers_per_shader_stage.min(cap);
        }
        let (device, queue) = regia_core::tasks::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("bindless path test"),
            required_limits: limits,
            required_features: wgpu::Features::empty(),
            memory_hints: crate::device::device_memory_hints(&adapter.get_info()),
            ..Default::default()
        }))
        .ok()?;
        let capabilities = crate::RenderCapabilities::from_adapter(&adapter, &device);
        Some(RenderDevice::from_parts(
            instance,
            adapter,
            device,
            queue,
            capabilities,
        ))
    }

    /// The acceptance check that needs no golden: the same frames on the storage-buffer path
    /// and on the uniform path (the same adapter, one storage buffer per stage too few) are
    /// byte-identical, and so are their draw counts. The scene covers textured and untextured
    /// materials, a blended quad, sun shadows and three frames of material parameter changes.
    #[test]
    fn storage_and_uniform_paths_draw_identical_pixels() {
        // Every frame complete (ADR-1584): while another test of this binary runs a warm queue
        // with workers, a deferred variant (the glass quad's blend pipeline) not built yet would
        // skip its draw for a frame on one path and not the other.
        let _complete = crate::pipeline_warm::blocking_scope();
        let (Some(ssbo), Some(uniform)) = (
            device_with_storage_cap(None),
            device_with_storage_cap(Some(super::MATERIAL_SSBO_MIN_STORAGE_BUFFERS - 1)),
        ) else {
            eprintln!("skipping: no GPU adapter (lavapipe) available");
            return;
        };
        if !super::material_ssbo(&ssbo.device) {
            eprintln!("skipping: the adapter is below the storage-buffer path itself");
            return;
        }
        assert!(
            !super::material_ssbo(&uniform.device),
            "the capped device takes the uniform path"
        );
        let checker = LabTexture {
            width: 2,
            height: 2,
            rgba: vec![
                255, 255, 255, 255, 40, 40, 40, 255, //
                40, 40, 40, 255, 255, 255, 255, 255,
            ],
        };
        let textured = LabMaterial {
            albedo: Some(checker),
            ..tinted(0.9, 0.3)
        };
        let glass = LabMaterial {
            base_color: [0.3, 0.6, 0.9, 0.5],
            alpha_mode: ALPHA_BLEND,
            ..LabMaterial::default()
        };
        let mut lab = scene(
            vec![tinted(0.8, 0.4), textured, glass],
            vec![
                quad(-1.0, 0.0, 0),
                quad(-0.3, 0.0, 1),
                quad(0.4, 0.0, 1),
                quad(1.1, 0.0, 0),
                quad(0.0, 0.5, 2),
            ],
            true,
        );
        let mut on_ssbo = FrameRenderer::new();
        let mut on_uniform = FrameRenderer::new();
        for frame in 0..3 {
            // A parameter change rewrites the record in place (no new bind group or buffer).
            lab.materials[0].roughness = 0.3 + 0.2 * frame as f32;
            let scene = lab.as_frame_scene_with_quality(FrameQuality::LAB);
            let a = on_ssbo
                .render_offscreen(&ssbo, &scene)
                .expect("storage path frame")
                .pixels;
            let b = on_uniform
                .render_offscreen(&uniform, &scene)
                .expect("uniform path frame")
                .pixels;
            assert_eq!(a.len(), b.len());
            let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
            assert_eq!(
                differing, 0,
                "frame {frame}: {differing} bytes differ between the paths"
            );
            assert_eq!(on_ssbo.draw_counts(), on_uniform.draw_counts(), "frame {frame}");
        }
        let arena = on_ssbo.material_arena_stats().expect("arena on the storage path");
        assert_eq!(arena.live, 3, "every standard material holds a slot");
        assert!(on_uniform.material_arena_stats().is_none());
    }
}
