// SPDX-License-Identifier: MPL-2.0
//! Projected decals: pooled, aged, faded — craters, scorch marks, impacts, pad markings.
//!
//! Decals are oriented boxes projected onto geometry through the depth buffer (`crate::gpu`'s
//! decal shader does the projection). The CPU side is a fixed-size pool: `spawn` overwrites the
//! oldest slot when full (budget per profile), every decal fades out over its lifetime and frees
//! its slot.
//!
//! A [`Decal`] is the oriented box (the M14 spawn shape, unchanged). A parallel [`DecalSurface`]
//! carries the material the shader writes — albedo, normal, roughness, optional emissive — plus
//! angle fade, distance fade, priority, the receiver layer mask and the atlas layer. `spawn`
//! fills that surface from the tint colour so existing callers keep the old look.

use regia_core::math::{Mat3, Mat4, Quat, Vec3, det_exp, det_sin_cos};
use regia_ecs::prelude::*;

/// Receiver layer bit for terrain (heightfield, ground).
pub const LAYER_TERRAIN: u32 = 1 << 0;
/// Receiver layer bit for static meshes (pads, props, vehicles).
pub const LAYER_STATIC_MESH: u32 = 1 << 1;
/// Receiver layer bit for characters. World decals leave this bit clear so they do not stick.
pub const LAYER_CHARACTER: u32 = 1 << 2;
/// Terrain and static meshes: the default mask for scorch, ruts, pad markings and grime.
pub const LAYER_WORLD: u32 = LAYER_TERRAIN | LAYER_STATIC_MESH;

/// Material channel: replace or modulate albedo.
pub const CHANNEL_ALBEDO: u32 = 1;
/// Material channel: relight with the receiver (or decal) normal.
pub const CHANNEL_NORMAL: u32 = 1 << 1;
/// Material channel: add a roughness-shaped specular lobe.
pub const CHANNEL_ROUGHNESS: u32 = 1 << 2;
/// Material channel: add emissive on top of the lit color.
pub const CHANNEL_EMISSIVE: u32 = 1 << 3;
/// Material channel: the decal's metallic (forward decals only; the post pass ignores it).
pub const CHANNEL_METALLIC: u32 = 1 << 4;

/// Which path shades a decal (ADR-3255).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecalShading {
    /// The forward pass when the forward-decal switch is on and the decal writes albedo,
    /// normal, roughness or metallic; the projected post pass otherwise.
    #[default]
    Auto,
    /// Always the projected post pass (relights the finished colour).
    Post,
}

/// Request a projected decal (send as a message; the VFX pool consumes it).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct SpawnDecal {
    /// Centre of the decal volume.
    pub position: Vec3,
    /// Orientation: local +Z is the projection direction (usually the surface normal).
    pub rotation: Quat,
    /// Half extents: `(u, v, depth)` in metres.
    pub half_size: Vec3,
    /// Linear RGBA tint. The alpha is the opacity.
    pub color: [f32; 4],
    /// Lifetime in seconds (`None` = until the pool recycles it).
    pub lifetime: Option<f32>,
}

/// Place a named decal prefab on a hit, aligned to the surface normal.
///
/// `prefab` is a [`crate::decal_format::DecalLibrary`] name (`scorch`, `pad_marking`, `rut`,
/// `grime`, or a loaded `.decal.ron`). Unknown names are skipped with a warning.
///
/// Apply one with [`place_decal`]. This is not an ECS message: registering another message
/// type changes when [`SpawnDecal`] buffers flush and splits arena replay.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaceDecal {
    /// Hit position, in metres (the centre of the volume).
    pub position: Vec3,
    /// Surface normal. Local +Z of the decal is aligned to this.
    pub normal: Vec3,
    /// Extra yaw in radians. [`align_to_normal`] spins the seed axis around world +Y and projects
    /// it onto the surface plane, so on a vertical wall the yaw does not turn the decal in-plane;
    /// a `StaticDecal` spins around its own local Z instead.
    pub yaw: f32,
    /// Prefab name.
    pub prefab: String,
    /// When `Some`, replaces the prefab lifetime.
    pub lifetime: Option<f32>,
}

/// One pooled decal box. The material lives in the parallel [`DecalSurface`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decal {
    /// Centre of the decal volume.
    pub position: Vec3,
    /// Orientation (local +Z projects into the surface).
    pub rotation: Quat,
    /// Half extents `(u, v, depth)`.
    pub half_size: Vec3,
    /// Linear tint colour. [`DecalSurface::from_decal`] copies this into the albedo.
    pub color: [f32; 4],
    /// Age in seconds.
    pub age: f32,
    /// Lifetime (infinite when `None`).
    pub lifetime: Option<f32>,
}

/// Material, fades, priority and receiver mask for one decal.
///
/// Defaults (via [`DecalSurface::from_decal`]) reproduce the M14 tint: albedo from the colour,
/// angle fade `0.25..0.6`, no distance fade, every receiver layer, no atlas, albedo channel only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecalSurface {
    /// Linear albedo.
    pub albedo: [f32; 3],
    /// Opacity multiplied with the lifetime fade, the atlas coverage and the fades.
    pub opacity: f32,
    /// `0` smooth, `1` rough. Used when [`CHANNEL_ROUGHNESS`] is set.
    pub roughness: f32,
    /// Tangent-space normal xy (z is reconstructed). Zero is a flat detail normal.
    pub normal_xy: [f32; 2],
    /// How strongly the receiver normal relights the decal (`0` = keep the geometric shade).
    pub normal_strength: f32,
    /// Linear emissive added when [`CHANNEL_EMISSIVE`] is set.
    pub emissive: [f32; 3],
    /// `smoothstep` range on `|dot(surface normal, projection axis)|`. `end <= start` disables it.
    pub angle_fade: [f32; 2],
    /// Distance from the camera, in metres, over which the decal fades out. `end <= start` disables it.
    pub distance_fade: [f32; 2],
    /// Draw order. Higher priority is drawn later (on top).
    pub priority: i16,
    /// Bit mask of receiver layers. `0` and `u32::MAX` accept every layer.
    pub receiver_mask: u32,
    /// Atlas array layer. Negative means a solid quad (no texture).
    pub atlas_layer: i32,
    /// [`CHANNEL_ALBEDO`] | [`CHANNEL_NORMAL`] | [`CHANNEL_ROUGHNESS`] | [`CHANNEL_EMISSIVE`].
    pub channels: u32,
    /// `0` hard box edge, `1` the M14 smoothstep feather.
    pub edge_softness: f32,
    /// `0` modulate the lit colour by the albedo, `1` replace it.
    pub blend: f32,
    /// Runtime only (not in `.decal.ron`): the emissive cools with the decal's age, as hot metal
    /// does, with this time constant in seconds (`0` = constant emissive). See
    /// [`cooled_emissive`].
    pub cooling_s: f32,
    /// Metallic written with [`CHANNEL_METALLIC`] (forward decals only).
    pub metallic: f32,
    /// Which path shades the decal ([`DecalShading::Auto`]: forward when it can).
    pub shading: DecalShading,
}

/// Emissive of a cooling glow `age` seconds after it was laid down, from its initial linear
/// `emissive` and time constant `tau` (`<= 0`: constant).
///
/// Hot metal radiates its heat away and the visible glow falls far faster than the temperature
/// (the visible band sits on the Wien tail, so it drops roughly as `exp(-c/T)`). Blue and green
/// fade first, so the spot goes white-yellow → orange → dull red → dark. Here the red channel
/// decays with `tau`, green with `tau / 1.25` and blue with `tau / 2.2`, a cheap black-body walk:
/// green never rises above red, and while the spot is bright it stays orange (the dim tail is the
/// dull red of cooling steel, on the hull, never over the sky).
#[must_use]
pub fn cooled_emissive(emissive: [f32; 3], tau: f32, age: f32) -> [f32; 3] {
    if tau <= 0.0 {
        return emissive;
    }
    let t = age.max(0.0);
    // The deterministic kernel: the three scales upload as decal uniforms, so platform `exp`
    // last digits would glow differently per machine.
    [
        emissive[0] * det_exp(-t / tau),
        emissive[1] * det_exp(-t * 1.25 / tau),
        emissive[2] * det_exp(-t * 2.2 / tau),
    ]
}

/// Ties a pooled decal to an entity (a hull, a building): the decal keeps its pose in the
/// entity's frame and is retired with the entity ([`DecalPool::follow_anchors`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecalAnchor {
    /// The entity the decal sits on.
    pub entity: Entity,
    /// Decal centre in the entity's local frame.
    pub local_position: Vec3,
    /// Decal orientation in the entity's local frame.
    pub local_rotation: Quat,
}

impl DecalAnchor {
    /// The anchor of a decal at world `position` / `rotation` on an entity whose world transform
    /// is `model` (translation, rotation; scale is ignored).
    #[must_use]
    pub fn from_world(entity: Entity, model: Mat4, position: Vec3, rotation: Quat) -> Self {
        let (_, model_rot, model_pos) = model.to_scale_rotation_translation();
        let inv = model_rot.inverse();
        Self {
            entity,
            local_position: inv * (position - model_pos),
            local_rotation: inv * rotation,
        }
    }
}

/// A stable handle to a pooled decal: its slot and the slot's generation. Retiring a decal (its
/// lifetime ran out, its anchor entity is gone, or the full pool recycled it) frees the slot and
/// bumps the generation, so an old handle never addresses the decal that later takes the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DecalId {
    /// Slot index (stable while the decal lives).
    pub slot: u32,
    /// The slot's generation when this decal took it.
    pub generation: u32,
}

impl DecalId {
    /// A total order over ids (slot, then generation), the order [`HiddenStaticDecals::ids`]
    /// keeps.
    #[must_use]
    pub fn sort_key(&self) -> (u32, u32) {
        (self.slot, self.generation)
    }
}

/// Reused buffers of [`DecalPool::visible_in_spawn_order`].
#[derive(Debug, Default, Clone)]
pub struct DecalDrawScratch {
    decals: Vec<Decal>,
    surfaces: Vec<DecalSurface>,
}

/// The pool decals of hidden (not parked) [`crate::StaticDecal`] entities this frame: their own
/// `Visibility::Hidden` or a hidden ancestor (`InheritedVisibility(false)`). The draw leaves them
/// out ([`DecalPool::visible_in_spawn_order`]); the pool keeps them, so showing the entity again
/// draws the same decal in its old place in the order (spec 14 § A1, ADR-7255). A parked decal
/// leaves the pool instead (spec 37 § 13).
#[derive(Resource, Debug, Default, Clone)]
pub struct HiddenStaticDecals {
    ids: Vec<DecalId>,
}

impl HiddenStaticDecals {
    /// The hidden decals, sorted by [`DecalId::sort_key`].
    #[must_use]
    pub fn ids(&self) -> &[DecalId] {
        &self.ids
    }

    /// Replaces the set with `ids` (any order), reusing the capacity.
    pub fn set(&mut self, ids: impl IntoIterator<Item = DecalId>) {
        self.ids.clear();
        self.ids.extend(ids);
        self.ids.sort_unstable_by_key(DecalId::sort_key);
        self.ids.dedup();
    }
}

/// Slot table entry: the slot's generation and where its decal sits in the dense arrays.
#[derive(Debug, Clone, Copy, Default)]
struct SlotEntry {
    generation: u32,
    dense: Option<u32>,
}

/// Fixed-size decal pool. Live decals sit in dense arrays (insertion order, surfaces and anchors
/// parallel) for the GPU upload; each also has a stable [`DecalId`] that survives other decals
/// retiring, so a live reload or an anchor addresses the right decal.
#[derive(Resource, Debug, Clone)]
pub struct DecalPool {
    decals: Vec<Decal>,
    surfaces: Vec<DecalSurface>,
    /// Entity anchors, parallel to `decals` (`None` = a world decal).
    anchors: Vec<Option<DecalAnchor>>,
    /// Stable ids, parallel to `decals`.
    ids: Vec<DecalId>,
    /// Spawn order, parallel to `decals` (the full pool recycles the oldest).
    serials: Vec<u64>,
    /// Slot table and its free list.
    slots: Vec<SlotEntry>,
    free: Vec<u32>,
    next_serial: u64,
    capacity: usize,
    /// Decals retired this frame (dropped from `decals`).
    retired: usize,
    /// Reused keep-mask for aging, anchor follow and removal. A steady frame clears and
    /// refills it; it allocates only when the live count grows past its capacity.
    keep_scratch: Vec<bool>,
    /// Footstep surface per decal (ADR-8829), parallel to `decals`: an index into
    /// `footstep_names`, or `None` when the decal leaves footsteps to its receiver.
    footsteps: Vec<Option<u16>>,
    /// Interned footstep surface names (grows only when a new name is set).
    footstep_names: Vec<std::sync::Arc<str>>,
}

/// Most distinct footstep surface names one [`DecalPool`] interns (ADR-8829).
pub const DECAL_FOOTSTEP_NAMES_MAX: usize = 1024;

impl DecalPool {
    /// Creates a pool with room for `capacity` decals.
    pub fn new(capacity: u32) -> Self {
        let n = capacity as usize;
        Self {
            decals: Vec::with_capacity(n),
            surfaces: Vec::with_capacity(n),
            anchors: Vec::with_capacity(n),
            ids: Vec::with_capacity(n),
            serials: Vec::with_capacity(n),
            slots: Vec::with_capacity(n),
            free: Vec::new(),
            next_serial: 0,
            capacity: n,
            retired: 0,
            keep_scratch: Vec::new(),
            footsteps: Vec::with_capacity(n),
            footstep_names: Vec::new(),
        }
    }

    /// Sets the footstep surface of live decal `id` (a v3 prefab's `surface`, ADR-8829); `None`
    /// clears it. False when `id` has retired, or when the pool already holds
    /// [`DECAL_FOOTSTEP_NAMES_MAX`] other names (nothing changes then). Presentation only:
    /// navigation and the simulation never read it.
    pub fn set_footstep_surface(&mut self, id: DecalId, surface: Option<&str>) -> bool {
        let Some(i) = self.index_of(id) else {
            return false;
        };
        let tag = match surface {
            None => None,
            Some(name) => match self.footstep_names.iter().position(|n| &**n == name) {
                Some(k) => Some(k as u16),
                None if self.footstep_names.len() < DECAL_FOOTSTEP_NAMES_MAX => {
                    self.footstep_names.push(std::sync::Arc::from(name));
                    Some((self.footstep_names.len() - 1) as u16)
                }
                None => return false,
            },
        };
        self.footsteps[i] = tag;
        true
    }

    /// The footstep surface of live decal `id`, if it has one.
    pub fn footstep_surface(&self, id: DecalId) -> Option<&str> {
        let i = self.index_of(id)?;
        self.footsteps[i].map(|k| &*self.footstep_names[k as usize])
    }

    /// The footstep surface at world `point` (ADR-8829): among live decals that carry a surface
    /// and whose box (`position`, `rotation`, `half_size`) contains `point`, the one with the
    /// highest [`DecalSurface::priority`], the newest on a tie (the decal the draw puts on top).
    /// `None` leaves the footstep to the receiver underneath (for example the terrain layer).
    /// A host footstep probe passes the answer as the event's surface.
    pub fn footstep_surface_at(&self, point: Vec3) -> Option<&str> {
        let _span = tracing::trace_span!("decal_footstep_surface_at").entered();
        let mut best: Option<(i16, usize)> = None;
        for (i, tag) in self.footsteps.iter().enumerate() {
            if tag.is_none() {
                continue;
            }
            let decal = &self.decals[i];
            let local = decal.rotation.inverse() * (point - decal.position);
            let inside = local.x.abs() <= decal.half_size.x
                && local.y.abs() <= decal.half_size.y
                && local.z.abs() <= decal.half_size.z;
            if !inside {
                continue;
            }
            let priority = self.surfaces[i].priority;
            // Dense order is spawn order, so a later index is newer and wins a tie.
            if best.is_none_or(|(p, _)| priority >= p) {
                best = Some((priority, i));
            }
        }
        let (_, i) = best?;
        self.footsteps[i].map(|k| &*self.footstep_names[k as usize])
    }

    /// Pool capacity.
    pub fn capacity(&self) -> u32 {
        self.capacity as u32
    }

    /// Live decals (aged but not yet faded out).
    pub fn len(&self) -> usize {
        self.decals.len()
    }

    /// True when no decal is live.
    pub fn is_empty(&self) -> bool {
        self.decals.is_empty()
    }

    /// Live decals, oldest first. A full pool rotates the oldest slot to the end and overwrites
    /// it, so this slice stays in spawn order after a wrap.
    pub fn decals(&self) -> &[Decal] {
        &self.decals
    }

    /// Material records parallel to [`DecalPool::decals`].
    pub fn surfaces(&self) -> &[DecalSurface] {
        &self.surfaces
    }

    /// Live decals and their surfaces in spawn order (oldest first), the order the draw uses to
    /// break priority ties so a newer decal lies over an older one of the same priority.
    ///
    /// The dense arrays stay in that order, including after a wrap, so this borrows them. It
    /// copies only if a caller has left `serials` unsorted.
    pub fn in_spawn_order(
        &self,
    ) -> (
        std::borrow::Cow<'_, [Decal]>,
        std::borrow::Cow<'_, [DecalSurface]>,
    ) {
        use std::borrow::Cow;
        if self.serials.is_sorted() {
            return (Cow::Borrowed(&self.decals), Cow::Borrowed(&self.surfaces));
        }
        let mut order: Vec<usize> = (0..self.decals.len()).collect();
        order.sort_by_key(|&i| self.serials[i]);
        (
            Cow::Owned(order.iter().map(|&i| self.decals[i]).collect()),
            Cow::Owned(order.iter().map(|&i| self.surfaces[i]).collect()),
        )
    }

    /// The live decals [`DecalPool::in_spawn_order`] lists minus the `hidden` ones (sorted by
    /// [`DecalId::sort_key`]): what the draw uploads (spec 14 § A1, ADR-7255). The pool itself is
    /// rollback state with an order (ADR-2728, ADR-4894), so hiding never removes or reorders an
    /// entry; the filter is here, at draw time. With nothing hidden this borrows the pool's arrays;
    /// otherwise it fills `scratch`, which keeps its capacity (a steady frame allocates nothing).
    pub fn visible_in_spawn_order<'a>(
        &'a self,
        hidden: &[DecalId],
        scratch: &'a mut DecalDrawScratch,
    ) -> (
        std::borrow::Cow<'a, [Decal]>,
        std::borrow::Cow<'a, [DecalSurface]>,
    ) {
        use std::borrow::Cow;
        if hidden.is_empty() {
            return self.in_spawn_order();
        }
        let is_hidden = |id: &DecalId| {
            hidden
                .binary_search_by_key(&id.sort_key(), DecalId::sort_key)
                .is_ok()
        };
        scratch.decals.clear();
        scratch.surfaces.clear();
        if self.serials.is_sorted() {
            for ((decal, surface), id) in self.decals.iter().zip(&self.surfaces).zip(&self.ids) {
                if !is_hidden(id) {
                    scratch.decals.push(*decal);
                    scratch.surfaces.push(*surface);
                }
            }
        } else {
            let mut order: Vec<usize> = (0..self.decals.len()).collect();
            order.sort_by_key(|&i| self.serials[i]);
            for i in order {
                if !is_hidden(&self.ids[i]) {
                    scratch.decals.push(self.decals[i]);
                    scratch.surfaces.push(self.surfaces[i]);
                }
            }
        }
        (
            Cow::Borrowed(scratch.decals.as_slice()),
            Cow::Borrowed(scratch.surfaces.as_slice()),
        )
    }

    /// Stable ids parallel to [`DecalPool::decals`].
    pub fn ids(&self) -> &[DecalId] {
        &self.ids
    }

    /// Entity anchors parallel to [`DecalPool::decals`] (`None` = a world decal).
    pub fn anchors(&self) -> &[Option<DecalAnchor>] {
        &self.anchors
    }

    /// The dense index of live decal `id`, or `None` when it has retired.
    pub fn index_of(&self, id: DecalId) -> Option<usize> {
        let entry = self.slots.get(id.slot as usize)?;
        if entry.generation != id.generation {
            return None;
        }
        entry.dense.map(|d| d as usize)
    }

    /// Live decal `id` and its surface.
    pub fn get(&self, id: DecalId) -> Option<(&Decal, &DecalSurface)> {
        let i = self.index_of(id)?;
        Some((&self.decals[i], &self.surfaces[i]))
    }

    /// Whether `id` is still live.
    pub fn contains(&self, id: DecalId) -> bool {
        self.index_of(id).is_some()
    }

    /// Adds a decal with a tint-only surface; recycles the oldest when the pool is full.
    pub fn spawn(&mut self, decal: Decal) {
        let surface = DecalSurface::from_decal(&decal);
        self.spawn_with(decal, surface);
    }

    /// Adds a decal and its material. Recycles the oldest decal when the pool is full (its id
    /// goes stale). Returns the new decal's stable id. A zero-capacity pool stores nothing and
    /// returns an id that is never live.
    pub fn spawn_with(&mut self, decal: Decal, surface: DecalSurface) -> DecalId {
        let _span = tracing::info_span!("decal_spawn").entered();
        self.spawn_inner(decal, surface, None)
    }

    /// Adds a decal that rides on `anchor`'s entity ([`Self::follow_anchors`] moves it each
    /// frame and retires it when the entity is gone). Recycles the oldest when full.
    pub fn spawn_anchored(&mut self, decal: Decal, surface: DecalSurface, anchor: DecalAnchor) -> DecalId {
        self.spawn_inner(decal, surface, Some(anchor))
    }

    fn new_slot(&mut self, dense: usize) -> DecalId {
        let slot = match self.free.pop() {
            Some(slot) => slot,
            None => {
                self.slots.push(SlotEntry::default());
                (self.slots.len() - 1) as u32
            }
        };
        let entry = &mut self.slots[slot as usize];
        entry.dense = Some(dense as u32);
        DecalId {
            slot,
            generation: entry.generation,
        }
    }

    fn free_slot(&mut self, id: DecalId) {
        if let Some(entry) = self.slots.get_mut(id.slot as usize)
            && entry.generation == id.generation
        {
            entry.generation = entry.generation.wrapping_add(1);
            entry.dense = None;
            self.free.push(id.slot);
        }
    }

    fn spawn_inner(&mut self, decal: Decal, surface: DecalSurface, anchor: Option<DecalAnchor>) -> DecalId {
        if self.capacity == 0 {
            return DecalId {
                slot: u32::MAX,
                generation: u32::MAX,
            };
        }
        let serial = self.next_serial;
        self.next_serial += 1;
        if self.decals.len() < self.capacity {
            let dense = self.decals.len();
            let id = self.new_slot(dense);
            self.decals.push(decal);
            self.surfaces.push(surface);
            self.anchors.push(anchor);
            self.ids.push(id);
            self.serials.push(serial);
            self.footsteps.push(None);
            id
        } else {
            // Full: the oldest decal is at index 0. Rotate it to the end and overwrite it, so
            // the dense arrays stay in spawn order and the upload can borrow them. `rotate_left`
            // is in place. The recycled id goes stale; survivors keep theirs.
            let n = self.decals.len();
            let old = self.ids[0];
            self.free_slot(old);
            self.decals.rotate_left(1);
            self.surfaces.rotate_left(1);
            self.anchors.rotate_left(1);
            self.ids.rotate_left(1);
            self.serials.rotate_left(1);
            self.footsteps.rotate_left(1);
            for i in 0..n - 1 {
                let id = self.ids[i];
                if let Some(entry) = self.slots.get_mut(id.slot as usize) {
                    entry.dense = Some(i as u32);
                }
            }
            let id = self.new_slot(n - 1);
            self.decals[n - 1] = decal;
            self.surfaces[n - 1] = surface;
            self.anchors[n - 1] = anchor;
            self.ids[n - 1] = id;
            self.serials[n - 1] = serial;
            self.footsteps[n - 1] = None;
            id
        }
    }

    /// Takes the reused keep-mask, cleared, with room for `len` flags.
    fn take_keep(&mut self, len: usize) -> Vec<bool> {
        let mut keep = std::mem::take(&mut self.keep_scratch);
        keep.clear();
        if keep.capacity() < len {
            keep.reserve(len);
        }
        keep
    }

    fn store_keep(&mut self, keep: Vec<bool>) {
        self.keep_scratch = keep;
    }

    /// Replaces live decal `id` (a live reload of its prefab). False when `id` has retired.
    pub fn replace(&mut self, id: DecalId, decal: Decal, surface: DecalSurface) -> bool {
        match self.index_of(id) {
            Some(i) => {
                self.decals[i] = decal;
                self.surfaces[i] = surface;
                true
            }
            None => false,
        }
    }

    /// Removes live decal `id` (its owner went away). False when `id` has already retired.
    pub fn remove(&mut self, id: DecalId) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        let mut keep = self.take_keep(self.decals.len());
        keep.resize(self.decals.len(), true);
        keep[index] = false;
        self.retain_dense(&keep);
        self.store_keep(keep);
        true
    }

    /// Adds a decal from a request (tint-only surface).
    pub fn spawn_request(&mut self, req: &SpawnDecal) {
        self.spawn(Decal {
            position: req.position,
            rotation: req.rotation,
            half_size: req.half_size,
            color: req.color,
            age: 0.0,
            lifetime: req.lifetime,
        });
    }

    /// Keeps the decals `keep` marks (dense order), retiring the rest: their slots are freed and
    /// the kept decals' slots point at their new dense indices. Returns how many retired.
    fn retain_dense(&mut self, keep: &[bool]) -> usize {
        let mut retired = 0;
        let mut w = 0;
        for r in 0..self.decals.len() {
            if keep.get(r).copied().unwrap_or(false) {
                if w != r {
                    self.decals[w] = self.decals[r];
                    self.surfaces[w] = self.surfaces[r];
                    self.anchors[w] = self.anchors[r];
                    self.ids[w] = self.ids[r];
                    self.serials[w] = self.serials[r];
                    self.footsteps[w] = self.footsteps[r];
                }
                let id = self.ids[w];
                if let Some(entry) = self.slots.get_mut(id.slot as usize) {
                    entry.dense = Some(w as u32);
                }
                w += 1;
            } else {
                let id = self.ids[r];
                self.free_slot(id);
                retired += 1;
            }
        }
        self.decals.truncate(w);
        self.surfaces.truncate(w);
        self.anchors.truncate(w);
        self.ids.truncate(w);
        self.serials.truncate(w);
        self.footsteps.truncate(w);
        retired
    }

    /// Re-expresses every decal after an origin rebase (spec 78 § 12.2). Anchored decals follow their
    /// entities anyway; this keeps them right on the rebase frame too.
    pub fn shift_origin(&mut self, shift: Vec3) {
        for decal in &mut self.decals {
            decal.position -= shift;
        }
    }

    /// Moves every anchored decal to its entity's current world transform (`world_of` returns
    /// it, or `None` when the entity is gone, which retires the decal).
    pub fn follow_anchors(&mut self, mut world_of: impl FnMut(Entity) -> Option<Mat4>) {
        if self.anchors.iter().all(Option::is_none) {
            return;
        }
        let mut keep = self.take_keep(self.decals.len());
        keep.resize(self.decals.len(), true);
        for (i, anchor) in self.anchors.iter().enumerate() {
            let Some(anchor) = anchor else { continue };
            match world_of(anchor.entity) {
                Some(model) => {
                    let (_, rot, pos) = model.to_scale_rotation_translation();
                    self.decals[i].position = pos + rot * anchor.local_position;
                    self.decals[i].rotation = rot * anchor.local_rotation;
                }
                None => keep[i] = false,
            }
        }
        if keep.iter().all(|k| *k) {
            self.store_keep(keep);
            return;
        }
        self.retain_dense(&keep);
        self.store_keep(keep);
    }

    /// Advances age, retires expired decals. Fade multiplier per decal is computed at draw time
    /// ([`Decal::fade`]). Surviving decals keep their ids.
    pub fn update(&mut self, dt: f32) {
        let _span = tracing::info_span!("decal_update").entered();
        let mut keep = self.take_keep(self.decals.len());
        for decal in &mut self.decals {
            decal.age += dt;
            keep.push(decal.lifetime.is_none_or(|life| decal.age < life));
        }
        self.retired = if keep.iter().all(|k| *k) {
            0
        } else {
            self.retain_dense(&keep)
        };
        self.store_keep(keep);
    }

    /// Decals retired by the last [`DecalPool::update`].
    pub fn retired_last_frame(&self) -> usize {
        self.retired
    }
}

impl Decal {
    /// Alpha multiplier: full until 70 % of life, then linear to zero ("aged" decals).
    pub fn fade(&self) -> f32 {
        match self.lifetime {
            None => 1.0,
            Some(life) if life <= 1e-6 => 0.0,
            Some(life) => {
                let t = self.age / life;
                if t < 0.7 {
                    1.0
                } else {
                    (1.0 - (t - 0.7) / 0.3).clamp(0.0, 1.0)
                }
            }
        }
    }
}

impl DecalSurface {
    /// Tint-only surface matching the M14 shader (grazing fade, soft edge, every layer).
    pub fn from_decal(decal: &Decal) -> Self {
        Self {
            albedo: [decal.color[0], decal.color[1], decal.color[2]],
            opacity: decal.color[3],
            roughness: 1.0,
            normal_xy: [0.0, 0.0],
            normal_strength: 0.0,
            emissive: [0.0, 0.0, 0.0],
            angle_fade: [0.25, 0.6],
            distance_fade: [0.0, 0.0],
            priority: 0,
            receiver_mask: u32::MAX,
            atlas_layer: -1,
            channels: CHANNEL_ALBEDO,
            edge_softness: 1.0,
            blend: 0.0,
            cooling_s: 0.0,
            metallic: 0.0,
            shading: DecalShading::Auto,
        }
    }
}

/// Hermite smoothstep, the same curve the decal shader uses.
pub fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if (edge1 - edge0).abs() <= 1e-8 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Angle fade from `|dot(normal, projection)|`. `end <= start` leaves the decal fully visible.
///
/// A decal that ignored the surface normal would return 1 here for every `ndot`.
pub fn angle_fade(ndot: f32, start: f32, end: f32) -> f32 {
    if end <= start {
        1.0
    } else {
        smoothstep(start, end, ndot.abs())
    }
}

/// Distance fade. `end <= start` disables it (returns 1).
pub fn distance_fade(distance: f32, start: f32, end: f32) -> f32 {
    if end <= start {
        1.0
    } else {
        1.0 - smoothstep(start, end, distance)
    }
}

/// Whether `mask` accepts receiver layer `layer_index` (0 = terrain, 1 = static mesh, 2 = character).
pub fn accepts_layer(mask: u32, layer_index: u32) -> bool {
    mask == 0 || mask == u32::MAX || mask & (1 << layer_index.min(31)) != 0
}

/// Orientation whose local +Z is `normal`. The yaw spins a seed axis around world +Y, which is then
/// projected onto the plane (not a spin around `normal`; use [`align_along`] to pick the in-plane
/// direction explicitly).
///
/// The yaw trig runs on the deterministic kernel ([`regia_core::math::det_sin_cos`]): the same
/// quaternion bits on every platform. Platform libm `sin`/`cos` last digits would otherwise store
/// and upload a different decal basis for the same yaw (spec 15 § 5).
pub fn align_to_normal(normal: Vec3, yaw: f32) -> Quat {
    let z = normal.normalize_or(Vec3::Y);
    // `Quat::from_rotation_y(yaw) * axis`, from the deterministic kernel: rotation about +Y
    // maps X to (cos, 0, -sin) and Z to (sin, 0, cos).
    let (sin, cos) = det_sin_cos(yaw);
    let spun = Vec3::new(cos, 0.0, -sin);
    let mut x = spun - z * spun.dot(z);
    if x.length_squared() < 1e-8 {
        let alt = Vec3::new(sin, 0.0, cos);
        x = alt - z * alt.dot(z);
    }
    x = x.normalize_or(Vec3::X);
    let y = z.cross(x).normalize_or(Vec3::Z);
    let x = y.cross(z).normalize_or(x);
    Quat::from_mat3(&Mat3::from_cols(x, y, z))
}

/// Orientation whose local +Z is `normal` and whose local +Y follows `tangent` on that plane.
pub fn align_along(normal: Vec3, tangent: Vec3) -> Quat {
    let z = normal.normalize_or(Vec3::Y);
    let y = tangent - z * tangent.dot(z);
    // A non-finite tangent (or one whose length overflows) has no usable direction: fall back as
    // for a degenerate one. Regression:
    // tests/tqs_sweep.rs::align_along_survives_degenerate_and_non_finite_tangents.
    let y = if y.length_squared() < 1e-8 {
        Vec3::ZERO
    } else {
        y.normalize_or_zero()
    };
    if y == Vec3::ZERO {
        return align_to_normal(z, 0.0);
    }
    let x = y.cross(z).normalize_or(Vec3::X);
    let y = z.cross(x).normalize_or(y);
    Quat::from_mat3(&Mat3::from_cols(x, y, z))
}

/// Places `place` into the world's [`DecalPool`] using the [`crate::decal_format::DecalLibrary`].
///
/// Returns the decal's stable id, or `None` when the pool is missing, the prefab is unknown, or
/// the prefab fails validation. Applies immediately (no per-frame system).
pub fn place_decal(world: &mut World, place: &PlaceDecal) -> Option<DecalId> {
    let prefab = world
        .get_resource::<crate::decal_format::DecalLibrary>()?
        .get(&place.prefab)?
        .clone();
    let (decal, surface) = prefab
        .instantiate(place.position, place.normal, place.yaw, place.lifetime)
        .ok()?;
    let mut pool = world.get_resource_mut::<DecalPool>()?;
    let id = pool.spawn_with(decal, surface);
    pool.set_footstep_surface(id, prefab.surface.as_deref());
    Some(id)
}

/// System: consume [`SpawnDecal`] messages into the [`DecalPool`].
pub fn apply_decal_requests(mut requests: MessageReader<SpawnDecal>, mut pool: ResMut<DecalPool>) {
    for req in requests.read() {
        pool.spawn_request(req);
    }
}

/// System: age the pool each frame.
pub fn update_decals(time: Res<regia_app::Time>, mut pool: ResMut<DecalPool>) {
    pool.update(time.delta_secs());
}

/// System: anchored decals ride their entities ([`DecalPool::follow_anchors`]).
pub fn follow_anchored_decals(
    pool: Option<ResMut<DecalPool>>,
    transforms: Query<&regia_scene::GlobalTransform>,
) {
    let Some(mut pool) = pool else { return };
    if pool.anchors.iter().all(Option::is_none) {
        return;
    }
    pool.follow_anchors(|entity| transforms.get(entity).ok().map(|t| Mat4::from(t.0)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rebase_moves_every_decal_by_the_shift() {
        let mut pool = DecalPool::new(4);
        pool.spawn(Decal {
            position: Vec3::new(5000.0, 1.0, 0.0),
            rotation: Quat::IDENTITY,
            half_size: Vec3::ONE,
            color: [1.0; 4],
            age: 0.0,
            lifetime: None,
        });
        pool.shift_origin(Vec3::new(4096.0, 0.0, 0.0));
        assert_eq!(pool.decals()[0].position, Vec3::new(904.0, 1.0, 0.0));
    }

    fn decal(lifetime: Option<f32>) -> Decal {
        Decal {
            position: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            half_size: Vec3::ONE,
            color: [1.0, 1.0, 1.0, 1.0],
            age: 0.0,
            lifetime,
        }
    }

    #[test]
    fn pool_overwrites_oldest_when_full() {
        let mut pool = DecalPool::new(3);
        for i in 0..5 {
            pool.spawn(Decal {
                position: Vec3::new(i as f32, 0.0, 0.0),
                ..decal(None)
            });
        }
        assert_eq!(pool.len(), 3, "capacity respected");
        let xs: Vec<f32> = pool.decals().iter().map(|d| d.position.x).collect();
        assert_eq!(xs, vec![2.0, 3.0, 4.0], "oldest first after the wrap");
    }

    #[test]
    fn remove_retires_one_decal_and_keeps_other_ids_valid() {
        let mut pool = DecalPool::new(4);
        let a = pool.spawn_with(decal(None), DecalSurface::from_decal(&decal(None)));
        let b = pool.spawn_with(decal(None), DecalSurface::from_decal(&decal(None)));
        let c = pool.spawn_with(decal(None), DecalSurface::from_decal(&decal(None)));
        assert!(pool.remove(a));
        assert!(!pool.remove(a), "a retired id removes nothing twice");
        assert_eq!(pool.len(), 2);
        assert!(!pool.contains(a));
        assert!(pool.contains(b) && pool.contains(c));
    }

    #[test]
    fn decals_age_and_retire() {
        let mut pool = DecalPool::new(8);
        pool.spawn(decal(Some(1.0)));
        pool.spawn(decal(None));
        assert_eq!(pool.retired_last_frame(), 0);
        pool.update(0.5);
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.retired_last_frame(), 0);
        pool.update(0.6);
        assert_eq!(pool.len(), 1, "timed decal retired");
        assert_eq!(pool.retired_last_frame(), 1);
        assert_eq!(pool.decals()[0].fade(), 1.0, "permanent decal never fades");
    }

    #[test]
    fn fade_curve_late_life() {
        let mut d = decal(Some(10.0));
        d.age = 5.0;
        assert_eq!(d.fade(), 1.0);
        d.age = 8.5;
        assert!((d.fade() - 0.5).abs() < 1e-6);
        d.age = 10.0;
        assert_eq!(d.fade(), 0.0);
    }

    #[test]
    fn angle_fade_uses_the_normal() {
        // Grazing (ndot ~ 0) is invisible; a facing normal is opaque. Ignoring the normal
        // would report 1 for both.
        assert!(angle_fade(0.0, 0.25, 0.6) < 1e-5);
        assert!((angle_fade(1.0, 0.25, 0.6) - 1.0).abs() < 1e-5);
        assert!(angle_fade(0.2, 0.3, 0.6) < 1e-5);
        assert!(angle_fade(0.9, 0.3, 0.6) > 0.99);
        assert!(
            (angle_fade(0.1, 0.0, 0.0) - 1.0).abs() < 1e-5,
            "end <= start disables"
        );
    }

    #[test]
    fn distance_fade_ramps_out() {
        assert!((distance_fade(0.0, 10.0, 20.0) - 1.0).abs() < 1e-5);
        assert!(distance_fade(20.0, 10.0, 20.0) < 1e-5);
        assert!(distance_fade(15.0, 10.0, 20.0) < 0.6 && distance_fade(15.0, 10.0, 20.0) > 0.4);
        assert!((distance_fade(100.0, 0.0, 0.0) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn layer_mask_rejects_characters() {
        assert!(accepts_layer(LAYER_WORLD, 0));
        assert!(accepts_layer(LAYER_WORLD, 1));
        assert!(!accepts_layer(LAYER_WORLD, 2), "characters are masked");
        assert!(accepts_layer(u32::MAX, 2));
        assert!(accepts_layer(0, 2), "0 accepts every layer");
    }

    #[test]
    fn align_to_normal_points_projection_along_the_hit() {
        let q = align_to_normal(Vec3::X, 0.0);
        let z = q * Vec3::Z;
        assert!(
            (z - Vec3::X).length() < 1e-4,
            "local +Z follows the normal, got {z}"
        );
        let up = align_to_normal(Vec3::Y, 0.0);
        assert!(((up * Vec3::Z) - Vec3::Y).length() < 1e-4);
        // The basis stays orthonormal.
        let x = up * Vec3::X;
        let y = up * Vec3::Y;
        assert!((x.dot(y)).abs() < 1e-4);
        assert!((x.cross(y) - Vec3::Y).length() < 1e-4);
        // A wall: yaw does not turn the decal 90 degrees around the normal.
        let wall_0 = align_to_normal(Vec3::X, 0.0);
        let wall_90 = align_to_normal(Vec3::X, std::f32::consts::FRAC_PI_2);
        assert!(((wall_0 * Vec3::Z) - Vec3::X).length() < 1e-4);
        assert!(((wall_90 * Vec3::Z) - Vec3::X).length() < 1e-4);
        assert!((wall_0 * Vec3::X).dot(wall_90 * Vec3::X) < -0.9);
        // A floor does turn, because the seed spin is already around the normal.
        let floor_0 = align_to_normal(Vec3::Y, 0.0) * Vec3::X;
        let floor_90 = align_to_normal(Vec3::Y, std::f32::consts::FRAC_PI_2) * Vec3::X;
        assert!(floor_0.dot(floor_90).abs() < 0.1);
    }

    /// The yaw basis is bit-pinned: it is built from the deterministic trig kernel, so the same
    /// yaw stores and uploads the same quaternion on every platform (a platform libm 1-ulp
    /// difference would change the composited decal).
    #[test]
    fn align_to_normal_yaw_bits_are_pinned() {
        // Yaw 0 is exact: the basis vectors are the coordinate axes, no trig survives.
        let zero = align_to_normal(Vec3::Y, 0.0);
        assert_eq!(
            zero.to_array().map(f32::to_bits),
            align_to_normal(Vec3::Y, 0.0).to_array().map(f32::to_bits),
            "yaw 0 is exact"
        );
        let q = align_to_normal(Vec3::Y, 0.7);
        assert_eq!(
            q.to_array().map(f32::to_bits),
            ALIGN_YAW_0_7_BITS,
            "yaw basis bits must hold on every target"
        );
        assert!(
            ((q * Vec3::Z) - Vec3::Y).length() < 1e-5,
            "still projects along +Y"
        );
        // A tilted normal too (the impact-site path).
        let tilted = align_to_normal(Vec3::new(0.3, 1.0, -0.2).normalize(), 2.1);
        assert_eq!(tilted.to_array().map(f32::to_bits), ALIGN_YAW_2_1_TILTED_BITS);
    }

    const ALIGN_YAW_0_7_BITS: [u32; 4] = [1059720044, 3195554006, 3195554006, 3207203692]; // pinned 2026-11-09
    const ALIGN_YAW_2_1_TILTED_BITS: [u32; 4] = [3197438040, 1060549631, 1056827865, 1053608597]; // pinned 2026-11-09

    #[test]
    fn ids_survive_other_decals_retiring_and_go_stale_with_their_own() {
        let mut pool = DecalPool::new(4);
        let short = pool.spawn_with(decal(Some(1.0)), DecalSurface::from_decal(&decal(None)));
        let keep = pool.spawn_with(
            Decal {
                position: Vec3::new(5.0, 0.0, 0.0),
                ..decal(None)
            },
            DecalSurface::from_decal(&decal(None)),
        );
        assert_eq!(pool.index_of(keep), Some(1));
        pool.update(2.0);
        assert_eq!(pool.len(), 1);
        assert!(!pool.contains(short), "a retired decal's id goes stale");
        assert_eq!(
            pool.index_of(keep),
            Some(0),
            "the survivor moved in the dense list"
        );
        // The freed slot is reused with a new generation: the stale id never reaches it.
        let new = pool.spawn_with(decal(None), DecalSurface::from_decal(&decal(None)));
        assert_eq!(new.slot, short.slot);
        assert_ne!(new.generation, short.generation);
        assert!(!pool.replace(short, decal(None), DecalSurface::from_decal(&decal(None))));
        let moved = Decal {
            position: Vec3::new(7.0, 0.0, 0.0),
            ..decal(None)
        };
        assert!(pool.replace(keep, moved, DecalSurface::from_decal(&moved)));
        assert_eq!(pool.get(keep).map(|(d, _)| d.position.x), Some(7.0));
        assert_eq!(pool.get(new).map(|(d, _)| d.position.x), Some(0.0));
    }

    #[test]
    fn a_full_pool_recycles_the_oldest_and_its_id_goes_stale() {
        let mut pool = DecalPool::new(2);
        let a = pool.spawn_with(decal(None), DecalSurface::from_decal(&decal(None)));
        let b = pool.spawn_with(decal(None), DecalSurface::from_decal(&decal(None)));
        let c = pool.spawn_with(decal(None), DecalSurface::from_decal(&decal(None)));
        assert!(!pool.contains(a) && pool.contains(b) && pool.contains(c));
        let d = pool.spawn_with(decal(None), DecalSurface::from_decal(&decal(None)));
        assert!(!pool.contains(b), "the next oldest goes next");
        assert!(pool.contains(c) && pool.contains(d));
        assert_eq!(pool.ids().len(), 2);
    }

    #[test]
    fn anchored_decals_retire_without_shifting_other_ids() {
        let mut world = World::new();
        let hull = world.spawn_empty().id();
        let mut pool = DecalPool::new(8);
        let anchored = pool.spawn_anchored(
            decal(None),
            DecalSurface::from_decal(&decal(None)),
            DecalAnchor {
                entity: hull,
                local_position: Vec3::ZERO,
                local_rotation: Quat::IDENTITY,
            },
        );
        let world_decal = pool.spawn_with(
            Decal {
                position: Vec3::new(3.0, 0.0, 0.0),
                ..decal(None)
            },
            DecalSurface::from_decal(&decal(None)),
        );
        pool.follow_anchors(|_| None);
        assert!(!pool.contains(anchored));
        assert_eq!(pool.get(world_decal).map(|(d, _)| d.position.x), Some(3.0));
    }

    #[test]
    fn spawn_with_keeps_surfaces_parallel_through_recycle() {
        let mut pool = DecalPool::new(2);
        let mut surface = DecalSurface::from_decal(&decal(None));
        surface.priority = 3;
        surface.receiver_mask = LAYER_WORLD;
        pool.spawn_with(decal(None), surface);
        pool.spawn(decal(None));
        pool.spawn_with(
            Decal {
                position: Vec3::new(9.0, 0.0, 0.0),
                ..decal(None)
            },
            DecalSurface {
                priority: 7,
                ..surface
            },
        );
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.surfaces().len(), pool.len());
        assert_eq!(pool.decals()[1].position.x, 9.0, "the newest decal is last");
        assert_eq!(pool.surfaces()[1].priority, 7);
        assert_eq!(pool.surfaces()[1].receiver_mask, LAYER_WORLD);
    }

    #[test]
    fn spawn_order_survives_a_wrap_for_priority_ties() {
        let mut pool = DecalPool::new(3);
        for i in 0..5 {
            pool.spawn(Decal {
                position: Vec3::new(i as f32, 0.0, 0.0),
                ..decal(None)
            });
        }
        let xs: Vec<f32> = pool.decals().iter().map(|d| d.position.x).collect();
        assert_eq!(
            xs,
            vec![2.0, 3.0, 4.0],
            "dense order stays spawn order after the wrap"
        );
        let (ordered, surfaces) = pool.in_spawn_order();
        assert!(matches!(ordered, std::borrow::Cow::Borrowed(_)));
        assert!(matches!(surfaces, std::borrow::Cow::Borrowed(_)));
        let xs: Vec<f32> = ordered.iter().map(|d| d.position.x).collect();
        assert_eq!(xs, vec![2.0, 3.0, 4.0], "oldest first, so the newest draws last");
        assert_eq!(surfaces.len(), ordered.len());
    }

    #[test]
    fn spawn_order_borrows_before_a_wrap() {
        let mut pool = DecalPool::new(4);
        pool.spawn(decal(None));
        pool.spawn(decal(None));
        let (ordered, surfaces) = pool.in_spawn_order();
        assert!(matches!(ordered, std::borrow::Cow::Borrowed(_)));
        assert!(matches!(surfaces, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn spawn_order_keeps_surfaces_paired_after_a_wrap() {
        let mut pool = DecalPool::new(2);
        for i in 0..3 {
            let d = Decal {
                position: Vec3::new(i as f32, 0.0, 0.0),
                ..decal(None)
            };
            let surface = DecalSurface {
                priority: i as i16,
                ..DecalSurface::from_decal(&d)
            };
            pool.spawn_with(d, surface);
        }
        let (ordered, surfaces) = pool.in_spawn_order();
        for (d, s) in ordered.iter().zip(surfaces.iter()) {
            assert_eq!(d.position.x as i16, s.priority, "surface follows its decal");
        }
        assert_eq!(ordered[0].position.x, 1.0);
    }

    #[test]
    fn blend_and_channels_default_to_the_modulating_albedo_tint() {
        let s = DecalSurface::from_decal(&decal(None));
        assert_eq!(s.channels, CHANNEL_ALBEDO);
        assert_eq!(s.blend, 0.0, "modulate, not replace");
        assert_eq!(s.priority, 0);
        assert_eq!(s.atlas_layer, -1);
    }

    #[test]
    fn cooled_emissive_walks_from_orange_to_dark_red() {
        let e = [4.0, 2.0, 1.0];
        assert_eq!(cooled_emissive(e, 0.0, 5.0), e, "tau <= 0 is constant");
        assert_eq!(cooled_emissive(e, 10.0, 0.0), e, "age 0 is the initial glow");
        let late = cooled_emissive(e, 10.0, 12.0);
        assert!(late[0] < e[0] && late[1] < e[1] && late[2] < e[2]);
        // Blue fades faster than green, green faster than red (relative to start).
        assert!(late[2] / e[2] < late[1] / e[1] && late[1] / e[1] < late[0] / e[0]);
        assert_eq!(cooled_emissive(e, 10.0, -3.0), e, "negative age clamps to 0");
        // The three channel scales upload as decal uniforms: their bits are pinned by the
        // deterministic exp kernel and must hold on every target.
        let glow = cooled_emissive([1.6, 0.35, 0.05], 10.0, 12.0);
        assert_eq!(
            glow.map(f32::to_bits),
            COOLED_EMISSIVE_PINNED_BITS,
            "cooled emissive bits changed"
        );
    }

    const COOLED_EMISSIVE_PINNED_BITS: [u32; 3] = [1056357632, 1033891984, 996791850]; // pinned 2026-11-09

    /// Review fix-vfx2-determinism finding 1: a crater-core ember decal (`cooling_s = 6.0`,
    /// `lifetime: None`) ages without bound; past roughly half an hour every channel scale
    /// underflows and the glow must reach exactly zero, never a negative or huge value.
    #[test]
    fn cooled_emissive_underflows_to_zero_at_extreme_age() {
        assert_eq!(cooled_emissive([4.0, 2.0, 1.0], 6.0, 100_000.0), [0.0; 3]);
        assert_eq!(cooled_emissive([4.0, 2.0, 1.0], 6.0, 3_600.0 * 24.0), [0.0; 3]);
        assert_eq!(cooled_emissive([4.0, 2.0, 1.0], 1.0, f32::MAX), [0.0; 3]);
    }

    #[test]
    fn smoothstep_degenerate_range_is_a_step() {
        assert_eq!(smoothstep(1.0, 1.0, 0.5), 0.0);
        assert_eq!(smoothstep(1.0, 1.0, 1.0), 1.0);
        assert!((smoothstep(0.0, 1.0, 0.5) - 0.5).abs() < 1e-6);
        assert_eq!(smoothstep(0.0, 1.0, -1.0), 0.0);
        assert_eq!(smoothstep(0.0, 1.0, 2.0), 1.0);
    }

    #[test]
    fn zero_lifetime_decal_is_invisible_and_retires() {
        let d = decal(Some(0.0));
        assert_eq!(d.fade(), 0.0);
        let mut pool = DecalPool::new(2);
        pool.spawn(d);
        pool.update(0.0);
        assert_eq!(pool.len(), 0, "age 0 >= lifetime 0 retires on the first update");
    }

    #[test]
    fn zero_capacity_pool_returns_a_dead_id() {
        let mut pool = DecalPool::new(0);
        let id = pool.spawn_with(decal(None), DecalSurface::from_decal(&decal(None)));
        assert!(pool.is_empty());
        assert!(!pool.contains(id));
    }
}
