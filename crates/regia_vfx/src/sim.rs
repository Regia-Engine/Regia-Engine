// SPDX-License-Identifier: MPL-2.0
//! CPU side of the particle simulation: instance lifetimes, deterministic spawn scheduling,
//! budgets and LOD.
//!
//! The GPU pool integrates particles ([`crate::gpu`]); this module decides **what spawns**. All
//! counts are exact integer maths on the CPU:
//!
//! - `Rate(n)` keeps a fractional accumulator — two runs with the same seed and timestep produce
//!   byte-identical spawn jobs (acceptance: "deterministic spawn counts").
//! - `PerDistance` emits one particle per `every` metres the instance origin moved (dust trails).
//! - `Burst(n)` fires once per activation.
//!
//! Budgets are enforced twice. The CPU refuses to schedule past `system.budget` live particles and
//! the profile's global headroom (the live count is exact for old-age deaths via per-emitter
//! death-time queues); the GPU pool is a ring that overwrites the oldest slot when full, so a spike
//! can never exceed memory. A scheduled job counts once: it is live from the frame it is
//! scheduled, whether or not the host has uploaded it yet.
//!
//! Particle attributes (shape position, velocity, lifetime, size, colour, spin) are sampled here
//! with [`Rng`] (PCG32, seeded per emitter) so a replay reproduces them exactly; the GPU only
//! integrates.

use std::sync::Arc;

#[cfg(test)]
thread_local! {
    /// Element moves of the death deque (insert shifts and discard scans).
    static DEATH_MOVES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Records `n` element moves of the death deque. Test-only; the counter is how the
/// varied-lifetime burst shows the old insert is quadratic.
#[cfg(test)]
fn note_death_moves(n: u64) {
    DEATH_MOVES.with(|c| c.set(c.get().saturating_add(n)));
}

/// Death-deque element moves since [`reset_death_moves`].
#[cfg(test)]
fn death_move_count() -> u64 {
    DEATH_MOVES.with(|c| c.get())
}

/// Clears the death-deque move counter.
#[cfg(test)]
fn reset_death_moves() {
    DEATH_MOVES.with(|c| c.set(0));
}

use regia_core::math::{Mat4, Rng, Vec3, det_cbrt, det_cos, det_sin_cos, det_tan};

use crate::mesh_shape::{SurfaceSample, VfxMesh};

/// Largest cone half-angle (radians) the spawn shape samples; `validate` rejects authored angles at or above 89 degrees.
const MAX_CONE_HALF_ANGLE: f32 = 1.55;

use crate::format::{BlendMode, InitModule, RangeDef, ShapeDef, SpawnMode, VelocityMode, VfxSystemDef};

/// One particle to insert into the GPU pool this frame (written into the spawn-job buffer).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpawnJob {
    /// World-space spawn position.
    pub position: [f32; 3],
    /// Initial velocity (m/s).
    pub velocity: [f32; 3],
    /// Lifetime in seconds (> 0).
    pub lifetime: f32,
    /// Base size in metres.
    pub size: f32,
    /// Initial rotation (radians).
    pub rotation: f32,
    /// Spin (radians/s).
    pub spin: f32,
    /// Initial linear RGBA colour.
    pub color: [f32; 4],
    /// Flipbook start phase in `0..1`. Zero unless the emitter set `random_start`.
    pub phase: f32,
}

/// LOD evaluation input: where the viewer is.
#[derive(Debug, Clone, Copy)]
pub struct LodInput {
    /// Camera position in world space.
    pub camera_pos: Vec3,
}

impl LodInput {
    /// Distance from the camera to a world point.
    fn distance(&self, p: Vec3) -> f32 {
        (p - self.camera_pos).length()
    }
}

/// Total order on particle death ages. Finite values keep `==` and `<=`, and `-0` equals `+0`.
///
/// The key is the standard signed-magnitude flip: negative bits are inverted, positive bits
/// flip the sign bit. That is the same order as `f32::total_cmp` for finite numbers once `-0`
/// has been normalized to `+0`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TimeKey(u32);

impl TimeKey {
    fn new(age: f32) -> Self {
        let age = if age == 0.0 { 0.0 } else { age };
        let bits = age.to_bits();
        let key = if bits & 0x8000_0000 != 0 {
            !bits
        } else {
            bits ^ 0x8000_0000
        };
        Self(key)
    }
}

/// A value stored in `OrderMap`: equal keys combine, and a zero value leaves the map.
trait Coalesce: Copy {
    /// `self` plus `extra`, saturating.
    fn coalesce_add(self, extra: Self) -> Self;
    /// `self` minus `extra`, saturating at zero.
    fn coalesce_sub(self, extra: Self) -> Self;
    /// No quantity left.
    fn coalesce_zero(self) -> bool;
}

impl Coalesce for u32 {
    fn coalesce_add(self, extra: Self) -> Self {
        self.saturating_add(extra)
    }

    fn coalesce_sub(self, extra: Self) -> Self {
        self.saturating_sub(extra)
    }

    fn coalesce_zero(self) -> bool {
        self == 0
    }
}

/// Absent child in `OrderMap`, and the end of the free list.
const ORDER_NONE: u32 = u32::MAX;

/// One node of a treap. Indices are into `OrderMap`'s `nodes`; `ORDER_NONE` is no child.
#[derive(Clone, Copy)]
struct OrderNode<V> {
    key: TimeKey,
    value: V,
    /// Heap priority. Lower bubbles toward the root. Derived from the key, so the shape is
    /// deterministic and a warm window can recycle nodes instead of allocating.
    prio: u32,
    left: u32,
    right: u32,
}

/// Ordered map with recycled nodes.
///
/// A `BTreeMap` allocates a node when a fresh death time arrives and frees it when that time
/// is popped, so a steady ribbon still allocates every few frames. This treap keeps every node
/// it has ever used and hands a freed index to the next insert. After the live set has reached
/// its high-water mark, insert and remove do not allocate.
///
/// The shape is a treap (Aragon and Seidel): the key order is the search tree, and `prio` is a
/// hash of the key so the expected depth is logarithmic. Equal keys coalesce; they do not
/// allocate a second node.
#[derive(Clone)]
struct OrderMap<V> {
    nodes: Vec<OrderNode<V>>,
    /// Intrusive free list. `ORDER_NONE` when every slot is live. The next free index lives in
    /// the free node's `left` field, so retiring a key does not allocate a side vector.
    free_head: u32,
    root: u32,
    len: usize,
}

impl<V> Default for OrderMap<V> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            free_head: ORDER_NONE,
            root: ORDER_NONE,
            len: 0,
        }
    }
}

impl<V: Coalesce> OrderMap<V> {
    fn len(&self) -> usize {
        self.len
    }

    fn prio(key: TimeKey) -> u32 {
        let mut x = key.0;
        x ^= x >> 16;
        x = x.wrapping_mul(0x7feb_352d);
        x ^= x >> 15;
        x = x.wrapping_mul(0x846c_a68b);
        x ^= x >> 16;
        x
    }

    fn alloc(&mut self, key: TimeKey, value: V) -> u32 {
        self.len += 1;
        let node = OrderNode {
            key,
            value,
            prio: Self::prio(key),
            left: ORDER_NONE,
            right: ORDER_NONE,
        };
        if self.free_head != ORDER_NONE {
            let index = self.free_head;
            self.free_head = self.nodes[index as usize].left;
            self.nodes[index as usize] = node;
            return index;
        }
        let index = self.nodes.len() as u32;
        // Spare capacity is taken while the live set is still growing (warm-up). A later frame
        // that only matches the high-water mark recycles a free index and does not allocate.
        if self.nodes.len() == self.nodes.capacity() {
            let extra = self.nodes.capacity().max(32);
            self.nodes.reserve(extra);
        }
        self.nodes.push(node);
        index
    }

    fn release(&mut self, index: u32) {
        self.len = self.len.saturating_sub(1);
        self.nodes[index as usize].left = self.free_head;
        self.nodes[index as usize].right = ORDER_NONE;
        self.free_head = index;
    }

    /// Adds `value` at `key`, or folds it into the value already stored there.
    fn insert(&mut self, key: TimeKey, value: V) {
        if value.coalesce_zero() {
            return;
        }
        self.root = self.insert_at(self.root, key, value);
    }

    fn insert_at(&mut self, root: u32, key: TimeKey, value: V) -> u32 {
        if root == ORDER_NONE {
            return self.alloc(key, value);
        }
        let node_key = self.nodes[root as usize].key;
        if key == node_key {
            let combined = self.nodes[root as usize].value.coalesce_add(value);
            self.nodes[root as usize].value = combined;
            return root;
        }
        let node_prio = self.nodes[root as usize].prio;
        if key < node_key {
            let child = self.insert_at(self.nodes[root as usize].left, key, value);
            self.nodes[root as usize].left = child;
            if self.nodes[child as usize].prio < node_prio {
                self.rotate_right(root)
            } else {
                root
            }
        } else {
            let child = self.insert_at(self.nodes[root as usize].right, key, value);
            self.nodes[root as usize].right = child;
            if self.nodes[child as usize].prio < node_prio {
                self.rotate_left(root)
            } else {
                root
            }
        }
    }

    fn rotate_right(&mut self, root: u32) -> u32 {
        let left = self.nodes[root as usize].left;
        let left_right = self.nodes[left as usize].right;
        self.nodes[left as usize].right = root;
        self.nodes[root as usize].left = left_right;
        left
    }

    fn rotate_left(&mut self, root: u32) -> u32 {
        let right = self.nodes[root as usize].right;
        let right_left = self.nodes[right as usize].left;
        self.nodes[right as usize].left = root;
        self.nodes[root as usize].right = right_left;
        right
    }

    /// Subtracts `value` from `key`. `false` when `key` is absent (the map is unchanged).
    /// A result of zero drops the key.
    fn try_sub(&mut self, key: TimeKey, value: V) -> bool {
        if value.coalesce_zero() {
            return false;
        }
        if !self.contains(key) {
            return false;
        }
        self.root = self.sub_at(self.root, key, value);
        true
    }

    fn contains(&self, key: TimeKey) -> bool {
        let mut at = self.root;
        while at != ORDER_NONE {
            let node = &self.nodes[at as usize];
            if key == node.key {
                return true;
            }
            at = if key < node.key { node.left } else { node.right };
        }
        false
    }

    fn sub_at(&mut self, root: u32, key: TimeKey, value: V) -> u32 {
        let node_key = self.nodes[root as usize].key;
        if key < node_key {
            let child = self.sub_at(self.nodes[root as usize].left, key, value);
            self.nodes[root as usize].left = child;
            root
        } else if key > node_key {
            let child = self.sub_at(self.nodes[root as usize].right, key, value);
            self.nodes[root as usize].right = child;
            root
        } else {
            let left = self.nodes[root as usize].value.coalesce_sub(value);
            if left.coalesce_zero() {
                self.delete_root(root)
            } else {
                self.nodes[root as usize].value = left;
                root
            }
        }
    }

    /// Rotates `root` down until it is a leaf, then releases it. Returns the new subtree root.
    fn delete_root(&mut self, root: u32) -> u32 {
        let left = self.nodes[root as usize].left;
        let right = self.nodes[root as usize].right;
        if left == ORDER_NONE && right == ORDER_NONE {
            self.release(root);
            return ORDER_NONE;
        }
        let left_prio = if left == ORDER_NONE {
            u32::MAX
        } else {
            self.nodes[left as usize].prio
        };
        let right_prio = if right == ORDER_NONE {
            u32::MAX
        } else {
            self.nodes[right as usize].prio
        };
        if left != ORDER_NONE && left_prio <= right_prio {
            let up = self.rotate_right(root);
            let child = self.delete_root(self.nodes[up as usize].right);
            self.nodes[up as usize].right = child;
            up
        } else {
            let up = self.rotate_left(root);
            let child = self.delete_root(self.nodes[up as usize].left);
            self.nodes[up as usize].left = child;
            up
        }
    }

    /// Pops the smallest key when it is `<= limit`.
    fn pop_le(&mut self, limit: TimeKey) -> Option<V> {
        let root = self.root;
        if root == ORDER_NONE {
            return None;
        }
        let mut at = root;
        while self.nodes[at as usize].left != ORDER_NONE {
            at = self.nodes[at as usize].left;
        }
        let key = self.nodes[at as usize].key;
        if key > limit {
            return None;
        }
        let value = self.nodes[at as usize].value;
        self.root = self.delete_root_key(self.root, key);
        Some(value)
    }

    fn delete_root_key(&mut self, root: u32, key: TimeKey) -> u32 {
        let node_key = self.nodes[root as usize].key;
        if key < node_key {
            let child = self.delete_root_key(self.nodes[root as usize].left, key);
            self.nodes[root as usize].left = child;
            root
        } else if key > node_key {
            let child = self.delete_root_key(self.nodes[root as usize].right, key);
            self.nodes[root as usize].right = child;
            root
        } else {
            self.delete_root(root)
        }
    }
}

/// Death ages of spawned particles, coalesced by equal time.
///
/// Insert, remove and pop are one tree operation each. Popping a time retires every particle
/// that dies then, which is the same live-count change as popping one record per particle.
/// Nodes are recycled, so a warm emitter does not allocate to record a new death time.
#[derive(Clone, Default)]
struct DeathSchedule {
    times: OrderMap<u32>,
}

impl DeathSchedule {
    fn insert(&mut self, age: f32, count: u32) {
        if count == 0 {
            return;
        }
        self.times.insert(TimeKey::new(age), count);
        #[cfg(test)]
        note_death_moves(1);
    }

    /// Drops one particle due at `age`. `false` when that time is not scheduled.
    fn remove_one(&mut self, age: f32) -> bool {
        if !self.times.try_sub(TimeKey::new(age), 1) {
            return false;
        }
        #[cfg(test)]
        note_death_moves(1);
        true
    }

    /// Retires every particle whose death age is `<= age`. Returns how many.
    fn pop_due(&mut self, age: f32) -> u32 {
        let limit = TimeKey::new(age);
        let mut retired = 0u32;
        while let Some(count) = self.times.pop_le(limit) {
            retired = retired.saturating_add(count);
            #[cfg(test)]
            note_death_moves(1);
        }
        retired
    }

    fn len(&self) -> usize {
        self.times.len()
    }
}

/// How many GPU child-ring particles one parent particle can still be responsible for.
///
/// `alpha` and `mesh` are list lengths. `soft` is a count; any positive value keeps the
/// soft layer on. A collision trigger stores `u32::MAX` for each class it can draw, and
/// the upload caps the total at the ring.
#[derive(Clone, Copy, Default)]
struct DrawHi {
    alpha: u32,
    mesh: u32,
    soft: u32,
}

impl DrawHi {
    fn is_zero(self) -> bool {
        self.alpha == 0 && self.mesh == 0 && self.soft == 0
    }

    fn saturating_add(self, other: Self) -> Self {
        Self {
            alpha: self.alpha.saturating_add(other.alpha),
            mesh: self.mesh.saturating_add(other.mesh),
            soft: self.soft.saturating_add(other.soft),
        }
    }

    fn saturating_mul(self, n: u32) -> Self {
        Self {
            alpha: self.alpha.saturating_mul(n),
            mesh: self.mesh.saturating_mul(n),
            soft: self.soft.saturating_mul(n),
        }
    }
}

/// Sum of [`DrawHi`] reservations over many parents.
///
/// 64-bit so the sum of per-parent `u32::MAX` collision reservations never saturates: a
/// saturated total minus one popped `u32::MAX` reservation would read zero while later
/// parents still hold theirs, and the upload would drop their children from the sort.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct ChildSum {
    alpha: u64,
    mesh: u64,
    soft: u64,
}

impl From<DrawHi> for ChildSum {
    fn from(hi: DrawHi) -> Self {
        Self {
            alpha: u64::from(hi.alpha),
            mesh: u64::from(hi.mesh),
            soft: u64::from(hi.soft),
        }
    }
}

impl Coalesce for ChildSum {
    fn coalesce_add(self, extra: Self) -> Self {
        Self {
            alpha: self.alpha.saturating_add(extra.alpha),
            mesh: self.mesh.saturating_add(extra.mesh),
            soft: self.soft.saturating_add(extra.soft),
        }
    }

    fn coalesce_sub(self, extra: Self) -> Self {
        Self {
            alpha: self.alpha.saturating_sub(extra.alpha),
            mesh: self.mesh.saturating_sub(extra.mesh),
            soft: self.soft.saturating_sub(extra.soft),
        }
    }

    fn coalesce_zero(self) -> bool {
        self.alpha == 0 && self.mesh == 0 && self.soft == 0
    }
}

/// Child-ring particles reserved by one emitter's already-spawned parents, by expiry time.
///
/// A parent particle reserves one firing per spawn or death link (`count` scaled by the
/// child's `count_scale`, the LOD band and the intensity, rounded up) until
/// `death_age + event_tail`. Probability below 1 still reserves the full count. The running
/// totals are what the scene uploads as the sort and soft bounds.
#[derive(Clone, Default)]
struct ChildSchedule {
    due: OrderMap<ChildSum>,
    /// Running totals of `due`. 64-bit: see [`ChildSum`].
    total: ChildSum,
}

impl ChildSchedule {
    fn insert(&mut self, age: f32, hi: DrawHi) {
        if hi.is_zero() || !age.is_finite() {
            return;
        }
        let hi = ChildSum::from(hi);
        self.due.insert(TimeKey::new(age), hi);
        self.total = self.total.coalesce_add(hi);
    }

    fn remove_one(&mut self, age: f32, hi: DrawHi) {
        if hi.is_zero() || !age.is_finite() {
            return;
        }
        let hi = ChildSum::from(hi);
        if !self.due.try_sub(TimeKey::new(age), hi) {
            return;
        }
        self.total = self.total.coalesce_sub(hi);
    }

    fn pop_due(&mut self, age: f32) {
        let limit = TimeKey::new(age);
        while let Some(hi) = self.due.pop_le(limit) {
            self.total = self.total.coalesce_sub(hi);
        }
    }
}

/// Draw class of one event-child particle. Mesh particles are not also counted as alpha:
/// the CPU `live_alpha` total already includes alpha-blended meshes, and the event extra
/// is added on top of that.
fn child_class(emitter: &crate::format::EmitterDef) -> DrawHi {
    let mesh = emitter.render.mode == crate::format::RenderMode::Mesh;
    let alpha = emitter.render.blend == BlendMode::Alpha && !mesh;
    DrawHi {
        alpha: u32::from(alpha),
        mesh: u32::from(mesh),
        soft: u32::from(child_is_soft(emitter)),
    }
}

/// Same predicate as [`crate::scene::is_soft_emitter`]. Duplicated so `sim` does not call
/// `scene` (that module already calls `sim`).
fn child_is_soft(emitter: &crate::format::EmitterDef) -> bool {
    use crate::format::SpriteId;
    let r = &emitter.render;
    r.blend == BlendMode::Alpha
        && r.softness >= 0.8
        && r.emissive <= 1.0
        && r.heat_haze <= 0.0
        && r.mode != crate::format::RenderMode::Mesh
        && r.mode != crate::format::RenderMode::Ribbon
        && matches!(r.sprite, SpriteId::Puff | SpriteId::PuffAnim | SpriteId::SoftDisc)
}

/// Children one particle of emitter `index` can have alive, including nested event links.
///
/// `depth` stops at [`crate::format::MAX_EVENT_DEPTH`]. A collision link reserves the whole
/// class (`u32::MAX`): contact can fire on every simulated step, and the upload caps the
/// sum at the child ring.
fn child_draw_upper(def: &VfxSystemDef, index: usize, lod: f32, intensity: f32, depth: usize) -> DrawHi {
    use crate::format::{EventTrigger, MAX_EVENT_DEPTH};
    if depth > MAX_EVENT_DEPTH {
        return DrawHi::default();
    }
    let Some(emitter) = def.emitters.get(index) else {
        return DrawHi::default();
    };
    let mut hi = DrawHi::default();
    for event in &emitter.update.events {
        let Some(child_index) = def.emitter_index(&event.child) else {
            continue;
        };
        let Some(child) = def.emitters.get(child_index) else {
            continue;
        };
        let class =
            child_class(child).saturating_add(child_draw_upper(def, child_index, lod, intensity, depth + 1));
        if event.on == EventTrigger::Collision {
            if class.alpha > 0 {
                hi.alpha = u32::MAX;
            }
            if class.mesh > 0 {
                hi.mesh = u32::MAX;
            }
            if class.soft > 0 {
                hi.soft = u32::MAX;
            }
            continue;
        }
        let raw = event.count as f32 * child.spawn.count_scale * lod * intensity;
        let n = if raw.is_finite() {
            raw.ceil().max(0.0) as u32
        } else {
            0
        };
        hi = hi.saturating_add(class.saturating_mul(n));
    }
    hi
}

/// Runtime state of one emitter inside an instance.
#[derive(Clone)]
struct EmitterState {
    /// Fractional rate accumulator (rate mode).
    rate_accum: f32,
    /// Distance accumulator (per-distance mode).
    distance_accum: f32,
    /// Whether this cycle's burst fired.
    burst_fired: bool,
    /// RNG stream for this emitter (deterministically forked from the instance seed).
    rng: Rng,
    /// Death ages of spawned particles — pops when the instance age passes them.
    deaths: DeathSchedule,
    /// Live-particle count (old-age model). Includes the `pending` jobs.
    live: u32,
    /// Jobs scheduled and not yet uploaded (a subset of `live`).
    pending: Vec<SpawnJob>,
    /// Death age of each entry of `pending` (same order), so dropping a pending job can also
    /// drop its death record.
    pending_deaths: Vec<f32>,
    /// Child-ring reservation of each pending job, parallel to `pending`. `None` when the
    /// job fires no events. Discarding the job drops the reservation; uploading it keeps the
    /// reservation until its expiry.
    pending_holds: Vec<Option<(f32, DrawHi)>>,
    /// GPU children the particles of this emitter can still have alive.
    child_hi: ChildSchedule,
    /// Spawns refused because the emitter's mesh shape has no usable mesh (v7).
    mesh_misses: u32,
}

impl EmitterState {
    /// Drops every pending job and its budget: they never became particles, so they stop counting
    /// as live and their death records go too.
    fn discard_pending(&mut self) {
        for death_age in self.pending_deaths.drain(..) {
            let _ = self.deaths.remove_one(death_age);
            self.live = self.live.saturating_sub(1);
        }
        for (until, per) in self.pending_holds.drain(..).flatten() {
            self.child_hi.remove_one(until, per);
        }
        self.pending.clear();
    }
}

/// Upper bound of an instance's intensity ([`VfxInstance::set_intensity`] clamps to it). The
/// `.vfx.ron` loader uses it to bound an event link's worst-case child count against the packed
/// link field ([`crate::events::MAX_PACKED_EVENT_COUNT`]).
pub const MAX_INSTANCE_INTENSITY: f32 = 4.0;

/// An active instance of a [`VfxSystemDef`] in the world.
#[derive(Clone)]
pub struct VfxInstance {
    /// Stable id (unique per [`crate::scene::VfxScene`]).
    pub(crate) id: u64,
    /// Index of the definition in the library.
    pub(crate) def_index: u32,
    /// Instance seed; forks per-emitter RNG streams deterministically.
    seed: u64,
    /// World-space origin.
    origin: Vec3,
    /// Local +Z in world space.
    forward: Vec3,
    /// Local +Y in world space.
    up: Vec3,
    /// Instance world velocity (velocity inheritance, per-distance spawn).
    velocity: Vec3,
    last_origin: Vec3,
    /// Distance the origin moved in the last [`Self::advance`] (per-distance spawns).
    moved: f32,
    /// Age since trigger (seconds).
    age: f32,
    /// Whether this instance still spawns (`false` = dying: emitters off, particles play out).
    active: bool,
    /// Set by [`Self::stop`]: the pending jobs are dropped instead of uploaded. A spent one-shot
    /// turns itself inactive without it, so its last jobs still become particles (ADR-1384).
    stopped: bool,
    /// Per-emitter runtime state, parallel to `def.emitters`.
    emitters: Vec<EmitterState>,
    /// Spatial scale of new particles ([`Self::set_scale`]).
    scale: f32,
    /// Spawn-count and opacity multiplier of new particles ([`Self::set_intensity`]).
    intensity: f32,
    /// LOD rate scale of the last schedule (event spawns scale their counts by it).
    last_lod_scale: f32,
    /// Instance age until which children of this instance's particles can still be alive: its
    /// emitter records must not be freed before it. Zero for a system without event spawns.
    hold_until: f32,
    /// Spawn meshes of mesh-shape emitters, by emitter index (v7, [`Self::bind_mesh`]).
    meshes: Vec<Option<Arc<VfxMesh>>>,
    /// Current joint palette of a skinned-mesh shape (v7, [`Self::set_skin_palette`]).
    palette: Vec<Mat4>,
    /// CPU ribbon tracks, by emitter index (v7; empty for other render modes).
    pub(crate) ribbons: Vec<crate::ribbon::RibbonTrack>,
}

/// Unit vector perpendicular to the unit `forward`, as close to `hint` as possible; falls back
/// to world Y or X when `hint` is (nearly) parallel to `forward`.
fn orthonormal_up(forward: Vec3, hint: Vec3) -> Vec3 {
    for candidate in [hint, Vec3::Y, Vec3::X] {
        let up = (candidate - forward * forward.dot(candidate)).normalize_or_zero();
        if up != Vec3::ZERO {
            return up;
        }
    }
    Vec3::X
}

impl VfxInstance {
    /// Moves the origin and every world-space spawn not yet uploaded by `-shift` (the floating-
    /// origin step of spec 78 § 12.2). The origin and the last origin move together, so neither
    /// the inherited velocity nor a per-distance spawn sees the jump.
    pub fn shift_origin(&mut self, shift: Vec3) {
        self.origin -= shift;
        self.last_origin -= shift;
        for emitter in &mut self.emitters {
            for job in &mut emitter.pending {
                for (axis, amount) in job.position.iter_mut().zip(shift.to_array()) {
                    *axis -= amount;
                }
            }
        }
    }

    /// Creates an instance at `origin` oriented along `forward` (fallback +Z) with `seed`.
    pub fn new(def_index: u32, seed: u64, origin: Vec3, forward: Vec3) -> Self {
        Self::with_id(0, def_index, seed, origin, forward)
    }

    /// Creates an instance with an explicit id (scenes assign these).
    pub fn with_id(id: u64, def_index: u32, seed: u64, origin: Vec3, forward: Vec3) -> Self {
        let forward = if forward.length_squared() < 1e-8 {
            Vec3::Z
        } else {
            forward.normalize()
        };
        let up = orthonormal_up(forward, Vec3::Y);
        Self {
            id,
            def_index,
            seed,
            origin,
            forward,
            up,
            velocity: Vec3::ZERO,
            last_origin: origin,
            moved: 0.0,
            age: 0.0,
            active: true,
            stopped: false,
            emitters: Vec::new(),
            scale: 1.0,
            intensity: 1.0,
            last_lod_scale: 1.0,
            hold_until: 0.0,
            meshes: Vec::new(),
            palette: Vec::new(),
            ribbons: Vec::new(),
        }
    }

    /// Gives emitter `index` its spawn mesh (a [`crate::format::ShapeDef::MeshSurface`] or
    /// [`crate::format::ShapeDef::SkinnedMesh`] shape); `None` unbinds it. A mesh-shape emitter
    /// without a mesh spawns nothing and counts the refused spawns ([`Self::mesh_misses`]).
    /// [`crate::scene::VfxScene::spawn`] binds the library's meshes.
    pub fn bind_mesh(&mut self, index: usize, mesh: Option<Arc<VfxMesh>>) {
        if self.meshes.len() <= index {
            self.meshes.resize(index + 1, None);
        }
        self.meshes[index] = mesh;
    }

    /// The spawn mesh bound to emitter `index`.
    #[must_use]
    pub fn mesh(&self, index: usize) -> Option<&VfxMesh> {
        self.meshes.get(index).and_then(Option::as_deref)
    }

    /// Sets the joint palette a skinned-mesh shape samples (one matrix per joint, bind pose to
    /// the instance frame). Copied into a buffer the instance keeps, so a per-frame call does not
    /// allocate once the joint count is reached.
    pub fn set_skin_palette(&mut self, palette: &[Mat4]) {
        self.palette.clear();
        self.palette.extend_from_slice(palette);
    }

    /// Joints in the current skin palette.
    #[must_use]
    pub fn skin_palette_len(&self) -> usize {
        self.palette.len()
    }

    /// Spawns emitter `index` refused because its mesh shape had no mesh bound.
    #[must_use]
    pub fn mesh_misses(&self, index: usize) -> u32 {
        self.emitters.get(index).map_or(0, |e| e.mesh_misses)
    }

    /// LOD rate scale of the last schedule (1 before the first).
    pub fn lod_scale(&self) -> f32 {
        self.last_lod_scale
    }

    /// Scales the instance's new particles spatially: spawn offsets and sizes by `scale`,
    /// launch speeds by `√scale` (Froude scaling: under the same gravity a flow `scale` times
    /// larger keeps geometrically similar trajectories at `√scale` the speed). One effect then
    /// fits a 3 m scout hull and a 20 m reclaimer. Clamped to `0.05..=20`; `1` is the authored
    /// effect.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = if scale.is_finite() {
            scale.clamp(0.05, 20.0)
        } else {
            1.0
        };
    }

    /// Spatial scale ([`Self::set_scale`]).
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Multiplies the instance's spawn counts by `intensity` and the opacity of new particles by
    /// `√min(intensity, 1)` (a thinner wash is both sparser and fainter). Above `1` only the counts
    /// grow: opacity and the instance's particle lights cap at their authored values. `0` spawns
    /// nothing while the instance stays alive; clamped to `0..=[`MAX_INSTANCE_INTENSITY`]`.
    /// Particles already alive keep their look.
    pub fn set_intensity(&mut self, intensity: f32) {
        self.intensity = if intensity.is_finite() {
            intensity.clamp(0.0, MAX_INSTANCE_INTENSITY)
        } else {
            0.0
        };
    }

    /// Spawn multiplier ([`Self::set_intensity`]).
    pub fn intensity(&self) -> f32 {
        self.intensity
    }

    /// Stable instance id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Index of the system definition this instance plays.
    pub fn def_index(&self) -> u32 {
        self.def_index
    }

    /// Updates the instance transform; the implied velocity feeds inheritance and trails.
    pub fn set_transform(&mut self, origin: Vec3, forward: Vec3, dt: f32) {
        if forward.length_squared() > 1e-8 {
            self.forward = forward.normalize();
            // Keep `up` perpendicular to the new heading so `axes` stays a rotation.
            self.up = orthonormal_up(self.forward, self.up);
        }
        self.velocity = if dt > 1e-5 {
            (origin - self.origin) / dt
        } else {
            Vec3::ZERO
        };
        self.origin = origin;
    }

    /// Instance world position.
    pub fn origin(&self) -> Vec3 {
        self.origin
    }

    /// Instance world velocity (m/s).
    pub fn velocity(&self) -> Vec3 {
        self.velocity
    }

    /// Local axes in world space: `(right, up, forward)`.
    pub fn axes(&self) -> (Vec3, Vec3, Vec3) {
        let right = self.forward.cross(self.up).normalize_or_zero();
        (right, self.up, self.forward)
    }

    /// Instance age in seconds since trigger.
    pub fn age(&self) -> f32 {
        self.age
    }

    /// Whether the instance can be recycled: inactive, every particle has died, and so has every
    /// child they can have fired (event spawns live on the GPU, so the bound is the longest
    /// child lifetime after the last parent death).
    pub fn is_dead(&self) -> bool {
        !self.active
            && self.age >= self.hold_until
            && self.emitters.iter().all(|e| e.live == 0 && e.pending.is_empty())
    }

    /// Whether the instance still spawns (`false` after [`Self::stop`]).
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Stops spawning; existing particles play out (jobs not uploaded yet are dropped).
    pub fn stop(&mut self) {
        self.active = false;
        self.stopped = true;
    }

    /// Live particles on alpha-blended emitters (the smoke budget).
    pub(crate) fn live_alpha(&self, def: &VfxSystemDef) -> u32 {
        self.emitters
            .iter()
            .enumerate()
            .map(|(i, state)| {
                let smoke = def
                    .emitters
                    .get(i)
                    .is_some_and(|emitter| emitter.render.blend == BlendMode::Alpha);
                if smoke { state.live } else { 0 }
            })
            .sum()
    }

    /// Live particles of emitter `index`, pending jobs included (old-age death model).
    pub(crate) fn emitter_live(&self, index: usize) -> u32 {
        self.emitters.get(index).map_or(0, |e| e.live)
    }

    /// Upper bound on GPU event-children this instance can still have alive: `(alpha, mesh, soft)`.
    ///
    /// Zero until a parent particle is scheduled. [`crate::scene::VfxScene::upload`] caps each
    /// total at the child ring and uses it for the alpha sort, the mesh sort and the soft layer.
    pub(crate) fn event_child_estimate(&self) -> (u32, u32, u32) {
        let sum = self.emitters.iter().fold(ChildSum::default(), |acc, emitter| {
            acc.coalesce_add(emitter.child_hi.total)
        });
        let clamp = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        (clamp(sum.alpha), clamp(sum.mesh), clamp(sum.soft))
    }

    /// Estimated live particles (exact under the old-age death model). A job scheduled and not
    /// yet uploaded counts once.
    pub fn live_particles(&self) -> u32 {
        self.emitters.iter().map(|e| e.live).sum()
    }

    /// Runs one frame of spawn scheduling; returns the number of jobs scheduled this frame.
    ///
    /// `headroom` is how many more particles the global profile budget can take this frame.
    /// Runs one frame of spawn scheduling. Public so off-screen tests and samples can drive
    /// individual instances without going through [`crate::scene::VfxScene`].
    pub fn schedule(&mut self, def: &VfxSystemDef, dt: f32, lod: &LodInput, headroom: u32) -> u32 {
        self.schedule_budgeted(def, dt, lod, headroom, u32::MAX)
    }

    /// Like [`VfxInstance::schedule`], and also refuses alpha-blended spawns past `smoke_headroom`.
    ///
    /// Additive and mesh particles do not consume the smoke budget. `u32::MAX` disables the cap
    /// (the bench stress path and [`VfxScene::update`](crate::scene::VfxScene::update)).
    pub fn schedule_budgeted(
        &mut self,
        def: &VfxSystemDef,
        dt: f32,
        lod: &LodInput,
        headroom: u32,
        smoke_headroom: u32,
    ) -> u32 {
        self.schedule_counted(def, dt, lod, headroom, smoke_headroom).0
    }

    /// [`VfxInstance::schedule_budgeted`] that also returns how many of the scheduled
    /// particles were alpha-blended, so a scene can charge both budgets across instances.
    pub(crate) fn schedule_counted(
        &mut self,
        def: &VfxSystemDef,
        dt: f32,
        lod: &LodInput,
        headroom: u32,
        smoke_headroom: u32,
    ) -> (u32, u32) {
        if !self.advance(def, dt) {
            return (0, 0);
        }
        self.spawn_counted(def, dt, lod, headroom, smoke_headroom)
    }

    /// The first half of a frame's scheduling: ages the instance by `dt`, retires the
    /// particles whose lifetime ended and drops stale pending jobs. A scene advances every
    /// instance before it measures the budgets, so a particle dying this frame frees its
    /// budget this frame. Returns `false` (and does nothing) when `dt <= 0` or `dt` is not finite.
    pub(crate) fn advance(&mut self, def: &VfxSystemDef, dt: f32) -> bool {
        if !dt.is_finite() || dt <= 0.0 {
            return false;
        }
        self.moved = (self.origin - self.last_origin).length();
        self.last_origin = self.origin;
        self.age += dt;

        if self.emitters.len() != def.emitters.len() {
            // A live reload changed the emitter count: keep the state (and death records) of
            // emitters that still exist so their GPU particles stay accounted for.
            let mut old = std::mem::take(&mut self.emitters).into_iter();
            self.emitters = def
                .emitters
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    old.next().unwrap_or_else(|| {
                        let rng =
                            Rng::with_stream(self.seed ^ (0x9e37_79b9_7f4a_7c15 + i as u64), i as u64 + 1);
                        EmitterState {
                            rate_accum: 0.0,
                            distance_accum: 0.0,
                            burst_fired: false,
                            rng,
                            deaths: DeathSchedule::default(),
                            live: 0,
                            pending: Vec::new(),
                            pending_deaths: Vec::new(),
                            pending_holds: Vec::new(),
                            child_hi: ChildSchedule::default(),
                            mesh_misses: 0,
                        }
                    })
                })
                .collect();
        }

        // A stopped instance never uploads its pending jobs; drop them so it can retire. A spent
        // one-shot keeps them: they are its particles, uploaded next (or aged out below).
        if self.stopped {
            for state in &mut self.emitters {
                state.discard_pending();
            }
        }

        // Retire particles whose lifetime expired.
        for state in &mut self.emitters {
            let retired = state.deaths.pop_due(self.age);
            state.live = state.live.saturating_sub(retired);
            state.child_hi.pop_due(self.age);
            // Jobs nobody uploaded (a host without a renderer, or one that skipped frames) are
            // part of `live`: once more of them wait than are alive, the oldest have died
            // unseen. Dropping them keeps the queue bounded by the budget. Their child
            // reservation stays: it expires on its own time, which is still an upper bound.
            let stale = state.pending.len().saturating_sub(state.live as usize);
            if stale > 0 {
                state.pending.drain(..stale);
                let deaths = stale.min(state.pending_deaths.len());
                state.pending_deaths.drain(..deaths);
                let holds = stale.min(state.pending_holds.len());
                state.pending_holds.drain(..holds);
            }
        }
        true
    }

    /// The second half of a frame's scheduling (after [`Self::advance`] with the same `dt`):
    /// spawns within the system budget, `headroom` and `smoke_headroom`. Returns
    /// `(scheduled, of which alpha-blended)`.
    pub(crate) fn spawn_counted(
        &mut self,
        def: &VfxSystemDef,
        dt: f32,
        lod: &LodInput,
        headroom: u32,
        smoke_headroom: u32,
    ) -> (u32, u32) {
        let moved = self.moved;
        let live_now = self.live_particles();
        let mut cap = def.system.budget.saturating_sub(live_now).min(headroom);
        let mut smoke_left = smoke_headroom;
        let lod_scale = def.lod_scale(lod.distance(self.origin));
        self.last_lod_scale = lod_scale;
        let mut scheduled = 0u32;
        let smoke_start = smoke_left;
        let (right, up, forward) = self.axes();
        let origin = self.origin;
        let velocity = self.velocity;

        for emitter_index in 0..def.emitters.len() {
            let emitter = &def.emitters[emitter_index];
            let state = &mut self.emitters[emitter_index];
            let activated = self.age >= emitter.delay;
            let window_open = match emitter.duration {
                Some(d) => self.age - emitter.delay <= d,
                None => true,
            };
            let mut count = 0u32;
            if self.active && activated && window_open && lod_scale > 0.0 && cap > 0 {
                let scale = emitter.spawn.count_scale * lod_scale * self.intensity;
                match emitter.spawn.mode {
                    SpawnMode::Burst(n) => {
                        if !state.burst_fired {
                            state.burst_fired = true;
                            count = ((n as f32) * scale).round() as u32;
                        }
                    }
                    SpawnMode::Rate(rate) => {
                        state.rate_accum += rate * scale * dt;
                        count = state.rate_accum as u32;
                        state.rate_accum -= count as f32;
                    }
                    SpawnMode::PerDistance { every } => {
                        state.distance_accum += moved * scale;
                        count = (state.distance_accum / every) as u32;
                        state.distance_accum -= count as f32 * every;
                    }
                    // Spawned by the parent's events, on the GPU (`crate::events`).
                    SpawnMode::Event => {}
                }
            }
            if !window_open {
                // Cycle ended (loop-style emitters re-burst next window).
                state.burst_fired = false;
                state.rate_accum = state.rate_accum.min(1.0);
            }
            count = count.min(cap);
            // Mesh shapes need their mesh (v7): without it the emitter spawns nothing.
            let mesh = self.meshes.get(emitter_index).and_then(Option::as_deref);
            match &emitter.init.shape {
                ShapeDef::MeshSurface { .. } | ShapeDef::SkinnedMesh { .. } if mesh.is_none() => {
                    state.mesh_misses = state.mesh_misses.saturating_add(count);
                    count = 0;
                }
                ShapeDef::SkinnedMesh { max_per_frame, .. } => count = count.min(*max_per_frame),
                _ => {}
            }
            let consumes_smoke = emitter.render.blend == BlendMode::Alpha;
            if consumes_smoke {
                count = count.min(smoke_left);
            }
            let reservation = {
                // `rate_scale` may be above 1. Clamping it would reserve fewer children than
                // `spawn_counted` can schedule, and the sort would drop the extras.
                let lod = if lod_scale.is_finite() {
                    lod_scale.max(0.0)
                } else {
                    0.0
                };
                let intensity = if self.intensity.is_finite() {
                    self.intensity.max(1.0)
                } else {
                    1.0
                };
                let per = child_draw_upper(def, emitter_index, lod, intensity, 0);
                if per.is_zero() { None } else { Some(per) }
            };
            if count > 0 && !emitter.update.events.is_empty() {
                // The longest this batch can keep the instance's records in use.
                let hold = self.age
                    + emitter.spawn.lifetime.max
                    + def.event_tail(emitter_index)
                    + crate::events::EVENT_HOLD_MARGIN_S;
                self.hold_until = self.hold_until.max(hold);
            }
            for _ in 0..count {
                let mut job = sample_job(
                    &emitter.init,
                    &emitter.spawn.lifetime,
                    state,
                    ShapeSource {
                        mesh,
                        palette: &self.palette,
                    },
                    origin,
                    velocity,
                    (right, up, forward),
                );
                if emitter.render.flipbook.is_some_and(|flip| flip.random_start) {
                    job.phase = state.rng.f32().clamp(0.0, 0.999);
                }
                if consumes_smoke {
                    job.size *= lod_size_compensation(lod_scale);
                }
                if self.scale != 1.0 || self.intensity != 1.0 {
                    tune_job(&mut job, origin, self.scale, self.intensity);
                }
                let death_age = self.age + job.lifetime;
                // Equal ages share one record. A later, shorter-lived particle still retires
                // on its own frame: the map is ordered by death time.
                state.deaths.insert(death_age, 1);
                state.live += 1;
                state.pending.push(job);
                state.pending_deaths.push(death_age);
                if let Some(per) = reservation {
                    let until = death_age + def.event_tail(emitter_index);
                    state.child_hi.insert(until, per);
                    state.pending_holds.push(Some((until, per)));
                } else {
                    state.pending_holds.push(None);
                }
                scheduled += 1;
            }
            cap = cap.saturating_sub(count);
            if consumes_smoke {
                smoke_left = smoke_left.saturating_sub(count);
            }
        }
        // A one-shot system (every emitter a fired burst or past its finite window) will never
        // spawn again: stop it so it retires once its particles die, instead of holding an
        // instance and GPU emitter records for the rest of the session.
        if self.active && !def.emitters.is_empty() {
            let age = self.age;
            let spent = def.emitters.iter().zip(&self.emitters).all(|(emitter, state)| {
                let window_closed = emitter.duration.is_some_and(|d| age - emitter.delay > d);
                match emitter.spawn.mode {
                    SpawnMode::Burst(_) => state.burst_fired || window_closed,
                    // Fired by its parents only, so it never keeps an instance spawning.
                    SpawnMode::Event => true,
                    _ => window_closed,
                }
            });
            if spent {
                self.active = false;
            }
        }
        (scheduled, smoke_start - smoke_left)
    }

    /// Drains all scheduled jobs into `out`.
    pub fn drain_jobs(&mut self, out: &mut Vec<SpawnJob>) {
        for e in &mut self.emitters {
            out.append(&mut e.pending);
            e.pending_deaths.clear();
            e.pending_holds.clear();
        }
    }

    /// Drains the jobs of emitter `index` only (needed to route jobs to GPU emitter slots).
    pub fn drain_emitter_jobs(&mut self, index: usize, out: &mut Vec<SpawnJob>) {
        if let Some(state) = self.emitters.get_mut(index) {
            out.append(&mut state.pending);
            state.pending_deaths.clear();
            state.pending_holds.clear();
        }
    }

    /// Hands emitter `index`'s pending jobs to `f` and clears them (the CPU ribbon path takes its
    /// jobs this way; the buffer keeps its capacity).
    pub(crate) fn with_emitter_jobs(&mut self, index: usize, f: impl FnOnce(&[SpawnJob])) {
        if let Some(state) = self.emitters.get_mut(index) {
            f(&state.pending);
            state.pending.clear();
            state.pending_deaths.clear();
            state.pending_holds.clear();
        }
    }

    /// Drops every pending job with its budget (live count and death record): for an instance
    /// whose jobs cannot be uploaded, so particles that never existed stop counting as live.
    pub(crate) fn discard_pending_jobs(&mut self) {
        for state in &mut self.emitters {
            state.discard_pending();
        }
    }

    /// Jobs waiting for upload.
    pub fn pending_jobs(&self) -> u32 {
        self.emitters.iter().map(|e| e.pending.len() as u32).sum()
    }

    /// `(total, largest per emitter)` death records kept for the old-age live count (stat:
    /// bounded by spawn rate × longest lifetime).
    pub(crate) fn death_records(&self) -> (usize, usize) {
        self.emitters.iter().fold((0, 0), |(total, most), e| {
            (total + e.deaths.len(), most.max(e.deaths.len()))
        })
    }
}

/// Applies an instance's [`VfxInstance::set_scale`] / [`VfxInstance::set_intensity`] to a job.
fn tune_job(job: &mut SpawnJob, origin: Vec3, scale: f32, intensity: f32) {
    let p = Vec3::from_array(job.position);
    job.position = (origin + (p - origin) * scale).to_array();
    let v = scale.sqrt();
    job.velocity = (Vec3::from_array(job.velocity) * v).to_array();
    job.size *= scale;
    job.color[3] *= intensity.min(1.0).sqrt();
}

/// Largest size multiplier [`lod_size_compensation`] applies.
pub const LOD_SIZE_CAP: f32 = 2.5;

/// Size multiplier for a smoke (alpha) particle spawned at LOD rate `scale`.
///
/// A far band that spawns `scale` as many puffs grows each one by `scale^-1/3` (capped at
/// [`LOD_SIZE_CAP`]), so a distant column keeps its body with fewer, larger puffs instead of
/// thinning until it vanishes. Separate puffs would need `1/sqrt(scale)` to keep count ×
/// size²; a smoke column overlaps itself, so that over-fills it (measured: 1.8× the near
/// coverage at 80 m), and the cube root lands near 1×.
pub fn lod_size_compensation(scale: f32) -> f32 {
    if scale >= 1.0 || scale <= 0.0 {
        return 1.0;
    }
    det_cbrt(1.0 / scale).min(LOD_SIZE_CAP)
}

/// What a mesh shape samples: the bound mesh and the instance's joint palette.
#[derive(Clone, Copy)]
pub(crate) struct ShapeSource<'a> {
    /// The emitter's spawn mesh, if bound.
    pub(crate) mesh: Option<&'a VfxMesh>,
    /// The current joint palette (empty: bind pose).
    pub(crate) palette: &'a [Mat4],
}

/// Samples one particle of an emitter's init module.
fn sample_job(
    init: &InitModule,
    lifetime: &RangeDef,
    state: &mut EmitterState,
    source: ShapeSource<'_>,
    origin: Vec3,
    inst_velocity: Vec3,
    axes: (Vec3, Vec3, Vec3),
) -> SpawnJob {
    let (right, up, forward) = axes;
    let rng = &mut state.rng;
    let (local, normal) = sample_shape_local(&init.shape, rng, source);
    let world = origin + right * local.x + up * local.y + forward * local.z;
    let velocity = sample_velocity(&init.velocity, rng, inst_velocity, axes, local, normal);
    let lifetime = rng.range(lifetime.min..=lifetime.max).max(0.01);
    let size = rng.range(init.size.min..=init.size.max).max(0.001);
    let rotation = rng.range(init.rotation.min..=init.rotation.max);
    let spin = rng.range(init.spin.min..=init.spin.max);
    let color = crate::curves::sample_color(&init.color, 0.0);
    SpawnJob {
        position: world.to_array(),
        velocity: velocity.to_array(),
        lifetime,
        size,
        rotation,
        spin,
        color,
        phase: 0.0,
    }
}

/// Samples a spawn position and the spawn normal in the emitter's local frame (+Z forward). The
/// normal costs no random draws, so the analytic shapes draw exactly what they drew in v6.
pub(crate) fn sample_shape_local(shape: &ShapeDef, rng: &mut Rng, source: ShapeSource<'_>) -> (Vec3, Vec3) {
    let outward = |p: Vec3| {
        let n = p.normalize_or_zero();
        if n == Vec3::ZERO { Vec3::Z } else { n }
    };
    match shape {
        ShapeDef::MeshSurface { .. } | ShapeDef::SkinnedMesh { .. } => {
            let Some(mesh) = source.mesh else {
                return (Vec3::ZERO, Vec3::Z);
            };
            let SurfaceSample { position, normal, .. } = if matches!(shape, ShapeDef::SkinnedMesh { .. }) {
                mesh.sample_skinned(rng, source.palette)
            } else {
                mesh.sample(rng)
            };
            (position, normal)
        }
        ShapeDef::Box {
            half_extents,
            surface: true,
        } => {
            let p = sample_analytic(shape, rng);
            let h = Vec3::new(half_extents.0, half_extents.1, half_extents.2);
            // The face the point lies on: the axis where it touches its half extent.
            let rel = (p.abs() - h).abs();
            let n = if rel.x <= rel.y && rel.x <= rel.z {
                Vec3::X * p.x.signum()
            } else if rel.y <= rel.z {
                Vec3::Y * p.y.signum()
            } else {
                Vec3::Z * p.z.signum()
            };
            (p, n)
        }
        _ => {
            let p = sample_analytic(shape, rng);
            (p, outward(p))
        }
    }
}

/// The v6 analytic shapes (a mesh shape is the origin).
fn sample_analytic(shape: &ShapeDef, rng: &mut Rng) -> Vec3 {
    match shape {
        ShapeDef::Point | ShapeDef::MeshSurface { .. } | ShapeDef::SkinnedMesh { .. } => Vec3::ZERO,
        ShapeDef::Sphere { radius, surface } => {
            let dir = rng.unit_vector();
            let r = if *surface {
                rng.range(radius.min..=radius.max)
            } else {
                rng.range(radius.min..=radius.max) * det_cbrt(rng.f32())
            };
            dir * r
        }
        ShapeDef::Box {
            half_extents,
            surface,
        } => {
            let h = Vec3::new(half_extents.0, half_extents.1, half_extents.2);
            if !*surface {
                Vec3::new(
                    rng.range(-h.x..=h.x),
                    rng.range(-h.y..=h.y),
                    rng.range(-h.z..=h.z),
                )
            } else {
                let face = rng.range(0u32..=5);
                let mut pick = |a: f32| rng.range(-a..=a);
                match face {
                    0 => Vec3::new(h.x, pick(h.y), pick(h.z)),
                    1 => Vec3::new(-h.x, pick(h.y), pick(h.z)),
                    2 => Vec3::new(pick(h.x), h.y, pick(h.z)),
                    3 => Vec3::new(pick(h.x), -h.y, pick(h.z)),
                    4 => Vec3::new(pick(h.x), pick(h.y), h.z),
                    _ => Vec3::new(pick(h.x), pick(h.y), -h.z),
                }
            }
        }
        ShapeDef::Cone { angle, length } => {
            let half = angle.to_radians().clamp(0.0, MAX_CONE_HALF_ANGLE);
            let l = rng.range(length.min..=length.max);
            // det_tan: the radius feeds replayed spawn jobs; platform libm tan would split
            // the bits between glibc and the Windows CRT (fix-vfx1-determinism finding 2).
            let max_r = l * det_tan(half);
            let r = max_r * rng.f32().sqrt();
            let phi = rng.range(0.0..std::f32::consts::TAU);
            let (sin, cos) = det_sin_cos(phi);
            Vec3::new(r * cos, r * sin, l)
        }
    }
}

/// Random unit direction within `spread` radians around `dir` (uniform cone sampling).
fn cone_spread(dir: Vec3, spread: f32, rng: &mut Rng) -> Vec3 {
    if spread <= 1e-5 || dir.length_squared() < 1e-10 {
        return dir.normalize_or_zero();
    }
    let z = rng.range(det_cos(spread)..=1.0);
    let phi = rng.range(0.0..std::f32::consts::TAU);
    let r = (1.0 - z * z).max(0.0).sqrt();
    let (sin, cos) = det_sin_cos(phi);
    let local = Vec3::new(r * cos, r * sin, z);
    let basis_up = if dir.z.abs() > 0.999 { Vec3::Y } else { Vec3::Z };
    let n = dir.normalize();
    let right = n.cross(basis_up).normalize();
    let up = right.cross(n);
    right * local.x + up * local.y + n * local.z
}

/// Samples the initial velocity in world space. `axes` is the instance's `(right, up, forward)`
/// (the frame the spawn offset `local` is rotated by).
///
/// `Radial` keeps its v1 meaning: the direction is the *unrotated* local offset, which mirrors
/// one axis against the spawn position (ADR-1398 records this; v4 effects were tuned on it).
/// `Planar` rotates the offset into the instance frame.
fn sample_velocity(
    vel: &crate::format::VelocityDef,
    rng: &mut Rng,
    inst_velocity: Vec3,
    axes: (Vec3, Vec3, Vec3),
    local: Vec3,
    normal: Vec3,
) -> Vec3 {
    let (right, up, inst_forward) = axes;
    let mut sample_speed = |r: &RangeDef| rng.range(r.min..=r.max);
    let (dir, s) = match &vel.mode {
        VelocityMode::Normal { speed } => (
            (right * normal.x + up * normal.y + inst_forward * normal.z).normalize_or_zero(),
            sample_speed(speed),
        ),
        VelocityMode::Radial { speed } => {
            let d = if local.length_squared() > 1e-10 {
                local.normalize()
            } else {
                inst_forward
            };
            (d, sample_speed(speed))
        }
        VelocityMode::Planar { speed, lift } => {
            let flat = right * local.x + up * local.y;
            let d = if flat.length_squared() > 1e-10 {
                flat.normalize()
            } else {
                let phi = sample_speed(&RangeDef {
                    min: 0.0,
                    max: std::f32::consts::TAU,
                });
                let (sin, cos) = det_sin_cos(phi);
                right * cos + up * sin
            };
            let out = sample_speed(speed);
            let rise = sample_speed(lift);
            let v = d * out + inst_forward * rise;
            let s = v.length();
            (v.normalize_or_zero(), s)
        }
        VelocityMode::Directional { dir, spread, speed } => {
            let d = crate::format::dir_from_tuple(*dir);
            // The instance basis: right × up × forward.
            let basis_up = if inst_forward.abs().dot(Vec3::Y) > 0.999 {
                Vec3::X
            } else {
                Vec3::Y
            };
            let right = inst_forward.cross(basis_up).normalize();
            let up = right.cross(inst_forward);
            let d = right * d.x + up * d.y + inst_forward * d.z;
            let s = sample_speed(speed);
            let d = if *spread > 0.0 {
                cone_spread(d, spread.clamp(0.0, 180.0).to_radians(), rng)
            } else {
                d
            };
            (d, s)
        }
    };
    dir * s + inst_velocity * vel.inherit
}

#[cfg(test)]
mod tests {
    #[test]
    fn non_finite_dt_does_not_poison_age() {
        let d = def(0.0, 100);
        let mut inst = VfxInstance::new(0, 1, Vec3::ZERO, Vec3::Z);
        assert!(inst.advance(&d, 0.25));
        assert!(!inst.advance(&d, f32::NAN));
        assert!(!inst.advance(&d, f32::INFINITY));
        assert_eq!(inst.age(), 0.25);
        assert!(inst.advance(&d, 0.25));
        assert_eq!(inst.age(), 0.5);
    }

    use super::*;
    use crate::format::{BlendMode, EmitterDef, RenderMode, SpawnModule};

    /// Radial flies along the unrotated local offset, not the offset after the instance basis.
    #[test]
    fn radial_velocity_follows_the_unrotated_local_offset() {
        let vel = crate::format::VelocityDef {
            mode: VelocityMode::Radial {
                speed: RangeDef::from(2.0),
            },
            inherit: 0.0,
        };
        let mut rng = Rng::new(1);
        let forward = Vec3::Z;
        let up = Vec3::Y;
        let right = forward.cross(up);
        let local = Vec3::X;
        let sampled = sample_velocity(&vel, &mut rng, Vec3::ZERO, (right, up, forward), local, Vec3::Y);
        let want = local.normalize() * 2.0;
        assert!((sampled - want).length() < 1e-4, "{sampled:?}");
        let rotated = right * local.x + up * local.y + forward * local.z;
        assert!(sampled.dot(rotated) < -0.9, "{sampled:?} vs {rotated:?}");
    }

    fn def(rate: f32, budget: u32) -> VfxSystemDef {
        VfxSystemDef {
            version: 1,
            name: "t".into(),
            system: crate::format::SystemModule {
                bounds: None,
                budget,
                lod: None,
            },
            emitters: vec![EmitterDef {
                name: "e".into(),
                delay: 0.0,
                duration: None,
                spawn: SpawnModule {
                    mode: SpawnMode::Rate(rate),
                    lifetime: RangeDef { min: 0.5, max: 0.5 },
                    count_scale: 1.0,
                },
                init: InitModule {
                    shape: ShapeDef::Sphere {
                        radius: RangeDef { min: 0.1, max: 0.1 },
                        surface: false,
                    },
                    velocity: Default::default(),
                    size: RangeDef { min: 0.5, max: 0.5 },
                    rotation: RangeDef { min: 0.0, max: 0.0 },
                    spin: RangeDef { min: 0.0, max: 0.0 },
                    color: Vec::new(),
                },
                update: Default::default(),
                render: crate::format::RenderModule {
                    mode: RenderMode::Billboard,
                    sprite: Default::default(),
                    flipbook: None,
                    blend: BlendMode::Alpha,
                    softness: 0.0,
                    alpha_scale: 1.0,
                    stretch: Default::default(),
                    light: None,
                    emissive: 1.0,
                    lit: false,
                    self_shadow: 0.65,
                    screen_floor_px: 0.0,
                    heat_haze: 0.0,
                    ribbon: Default::default(),
                    mesh: None,
                },
            }],
        }
    }

    const DT: f32 = 1.0 / 60.0;
    const LOD: LodInput = LodInput {
        camera_pos: Vec3::ZERO,
    };
    /// FNV-1a over the seed-77 job bits of `moving_instance_jobs_are_reproducible`, from the
    /// deterministic det kernels; a platform libm drifts from this hash. Re-recorded
    /// intentionally (fixrev-22-k, Muse review finding 5): the test's own path drove the
    /// instance along the platform `(t * 3.0).sin()` before; now that it uses `det_sin_cos`
    /// the pin is itself platform-independent.
    const SEED77_JOBS_BITS_HASH: u64 = 0x8a0d_331b_8d57_8ba0;

    fn drain(inst: &mut VfxInstance) -> Vec<SpawnJob> {
        let mut out = Vec::new();
        inst.drain_jobs(&mut out);
        out
    }

    #[test]
    fn a_non_finite_dt_does_not_age_the_instance() {
        let d = def(60.0, 10_000);
        let mut inst = VfxInstance::new(0, 1, Vec3::ZERO, Vec3::Z);
        for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, 0.0] {
            assert!(!inst.advance(&d, dt), "dt {dt} is skipped");
        }
        assert_eq!(inst.age, 0.0);
        assert!(inst.advance(&d, DT));
        assert!((inst.age - DT).abs() < 1e-9);
    }

    #[test]
    fn spread_is_clamped_to_a_half_sphere() {
        let mut rng = Rng::new(3);
        // 360 degrees would fold to a single ray through cos; clamped it covers the sphere.
        let mut seen_back = false;
        for _ in 0..200 {
            let v = cone_spread(Vec3::Z, 180.0f32.to_radians(), &mut rng);
            seen_back |= v.z < 0.0;
        }
        assert!(seen_back);
    }

    #[test]
    fn rate_scheduling_is_exact_and_deterministic() {
        let d = def(120.0, 10_000); // 2 particles per frame at 60 Hz
        let run = || {
            let mut inst = VfxInstance::new(0, 99, Vec3::new(5.0, 0.0, 0.0), Vec3::Z);
            let mut total = Vec::new();
            for _ in 0..60 {
                inst.schedule(&d, DT, &LOD, u32::MAX);
                total.append(&mut drain(&mut inst));
            }
            total
        };
        let a = run();
        let b = run();
        assert_eq!(a.len(), 120, "120/s over 1 s at 60 Hz");
        assert_eq!(a, b, "same seed → identical jobs");
    }

    /// Spec 78 § 12.2, ADR-2022: a rebase moves the origin and the pending spawns with it, so an
    /// instance following an entity sees its steady velocity, not the jump.
    #[test]
    fn a_rebase_moves_the_origin_and_pending_spawns_without_a_velocity_spike() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(3);
        let mut inst = VfxInstance::new(0, 7, Vec3::new(4100.0, 0.0, 0.0), Vec3::Z);
        inst.set_transform(Vec3::new(4101.0, 0.0, 0.0), Vec3::Z, DT);
        assert!((inst.velocity().x - 1.0 / DT).abs() < 1e-2);
        assert_eq!(inst.schedule(&d, DT, &LOD, u32::MAX), 3);
        let before: Vec<[f32; 3]> = inst.emitters[0].pending.iter().map(|j| j.position).collect();
        let shift = Vec3::new(4096.0, 0.0, 0.0);
        inst.shift_origin(shift);
        assert_eq!(inst.origin(), Vec3::new(5.0, 0.0, 0.0));
        let after: Vec<[f32; 3]> = inst.emitters[0].pending.iter().map(|j| j.position).collect();
        for (a, b) in before.iter().zip(&after) {
            assert_eq!(a[0] - 4096.0, b[0]);
            assert_eq!((a[1], a[2]), (b[1], b[2]));
        }
        // The followed entity's next local position, one meter on.
        inst.set_transform(Vec3::new(6.0, 0.0, 0.0), Vec3::Z, DT);
        assert!(
            (inst.velocity().x - 1.0 / DT).abs() < 1e-2,
            "{}",
            inst.velocity().x
        );
    }

    /// Jobs that cannot be uploaded (no GPU emitter records) are discarded with their budget,
    /// while a running instance keeps spawning afterwards.
    #[test]
    fn discarded_pending_jobs_release_their_budget() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(5);
        let mut inst = VfxInstance::new(0, 4, Vec3::ZERO, Vec3::Z);
        assert_eq!(inst.schedule(&d, DT, &LOD, u32::MAX), 5);
        assert!(inst.live_particles() > 0);
        inst.discard_pending_jobs();
        assert_eq!(inst.pending_jobs(), 0);
        assert_eq!(inst.live_particles(), 0, "never-uploaded jobs are not live");
        assert_eq!(inst.death_records().0, 0, "their death records go too");
    }

    /// Stopping before the pending jobs upload drops them with their budget: the instance
    /// retires at once instead of waiting out the lifetimes of particles that never existed.
    #[test]
    fn stop_with_pending_jobs_releases_their_budget() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(7); // lifetime 0.5 s
        let mut inst = VfxInstance::new(0, 3, Vec3::ZERO, Vec3::Z);
        assert_eq!(inst.schedule(&d, DT, &LOD, u32::MAX), 7);
        assert_eq!(inst.pending_jobs(), 7);
        inst.stop();
        inst.schedule(&d, DT, &LOD, u32::MAX);
        assert_eq!(inst.pending_jobs(), 0);
        assert_eq!(inst.live_particles(), 0);
        assert!(inst.is_dead());
    }

    /// A spent one-shot stops itself without dropping its last jobs: a host that has not
    /// uploaded them yet (or never does, headless) still counts them live until they would
    /// have died, then the instance retires.
    #[test]
    fn a_spent_burst_keeps_its_pending_jobs_until_they_age_out() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(6); // lifetime 0.5 s
        let mut inst = VfxInstance::new(0, 21, Vec3::ZERO, Vec3::Z);
        assert_eq!(inst.schedule(&d, DT, &LOD, u32::MAX), 6);
        assert!(!inst.is_active(), "the burst fired: nothing left to spawn");
        for _ in 0..3 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
        }
        assert_eq!(inst.pending_jobs(), 6, "not uploaded, not dropped");
        assert_eq!(inst.live_particles(), 6);
        assert!(!inst.is_dead());
        for _ in 0..60 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
        }
        assert_eq!(inst.live_particles(), 0);
        assert_eq!(inst.pending_jobs(), 0, "aged out unseen");
        assert!(inst.is_dead());
    }

    #[test]
    fn spent_burst_instance_retires_without_an_explicit_stop() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(8);
        let mut inst = VfxInstance::new(0, 11, Vec3::ZERO, Vec3::Z);
        let mut jobs = 0;
        for _ in 0..120 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            jobs += drain(&mut inst).len();
        }
        assert_eq!(jobs, 8, "the burst fires exactly once");
        assert!(
            inst.is_dead(),
            "particles (0.5 s) are gone, so the instance retires"
        );
    }

    #[test]
    fn open_ended_rate_instance_keeps_running() {
        let d = def(60.0, 10_000);
        let mut inst = VfxInstance::new(0, 12, Vec3::ZERO, Vec3::Z);
        for _ in 0..240 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
        }
        assert!(!inst.is_dead());
    }

    #[test]
    fn rate_instance_with_a_finite_duration_retires() {
        let mut d = def(60.0, 10_000);
        d.emitters[0].duration = Some(0.25);
        let mut inst = VfxInstance::new(0, 13, Vec3::ZERO, Vec3::Z);
        for _ in 0..240 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
        }
        assert!(inst.is_dead());
    }

    #[test]
    fn fractional_rates_accumulate() {
        let d = def(30.0, 10_000); // 0.5/frame
        let mut inst = VfxInstance::new(0, 1, Vec3::ONE, Vec3::Z);
        let mut total = 0;
        for _ in 0..10 {
            total += inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
        }
        assert_eq!(total, 5);
    }

    #[test]
    fn reload_with_more_emitters_keeps_existing_death_records() {
        let d = def(600.0, 10_000);
        let mut inst = VfxInstance::new(0, 21, Vec3::ZERO, Vec3::Z);
        for _ in 0..10 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
        }
        let live = inst.emitter_live(0);
        assert!(live > 0);
        let mut grown = d.clone();
        grown.emitters.push(d.emitters[0].clone());
        grown.emitters[1].name = "e2".into();
        grown.emitters[1].spawn.mode = SpawnMode::Rate(0.0);
        grown.emitters[0].spawn.mode = SpawnMode::Rate(0.0);
        inst.schedule(&grown, DT, &LOD, u32::MAX);
        assert_eq!(inst.emitter_live(0), live, "emitter 0 keeps its live particles");
        inst.stop();
        assert!(!inst.is_dead(), "particles are still alive");
    }

    #[test]
    fn burst_fires_once() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(7);
        let mut inst = VfxInstance::new(0, 3, Vec3::ZERO, Vec3::Z);
        let n1 = inst.schedule(&d, DT, &LOD, u32::MAX);
        let n2 = inst.schedule(&d, DT, &LOD, u32::MAX);
        let n3 = inst.schedule(&d, DT, &LOD, u32::MAX);
        assert_eq!((n1, n2, n3), (7, 0, 0));
    }

    #[test]
    fn per_distance_emits_on_travel() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::PerDistance { every: 2.5 };
        let mut inst = VfxInstance::new(0, 4, Vec3::ZERO, Vec3::Z);
        inst.schedule(&d, DT, &LOD, u32::MAX);
        drain(&mut inst);
        inst.set_transform(Vec3::new(1.0, 0.0, 0.0), Vec3::Z, DT);
        let n1 = inst.schedule(&d, DT, &LOD, u32::MAX); // 1 m moved → 0
        inst.set_transform(Vec3::new(3.0, 0.0, 0.0), Vec3::Z, DT); // +2 m → total 3 m
        let n2 = inst.schedule(&d, DT, &LOD, u32::MAX);
        inst.set_transform(Vec3::new(6.0, 0.0, 0.0), Vec3::Z, DT); // +3 m → total 6 m
        let n3 = inst.schedule(&d, DT, &LOD, u32::MAX);
        assert_eq!((n1, n2, n3), (0, 1, 1));
    }

    #[test]
    fn budget_caps_live_particles() {
        let d = def(1000.0, 100); // 16.6/frame; 0.5 s lifetime
        let mut inst = VfxInstance::new(0, 5, Vec3::ZERO, Vec3::Z);
        let mut peak = 0u32;
        for _ in 0..120 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
            peak = peak.max(inst.live_particles());
        }
        assert!(peak <= 100, "peak {peak} must respect system budget");
        assert!(peak >= 90, "budget should be nearly reachable, peak {peak}");
    }

    #[test]
    fn deaths_free_budget_again() {
        let d = def(1000.0, 100);
        let mut inst = VfxInstance::new(0, 6, Vec3::ZERO, Vec3::Z);
        for _ in 0..30 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
        }
        let after_30 = inst.live_particles();
        assert_eq!(after_30, 100, "saturated at budget");
        for _ in 0..60 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
        }
        // Still saturated but scheduling resumed (not deadlocked at cap).
        assert_eq!(inst.live_particles(), 100);
        inst.stop();
        for _ in 0..60 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
        }
        assert_eq!(inst.live_particles(), 0, "all particles retired");
        assert!(inst.is_dead());
    }

    #[test]
    fn global_headroom_limits_scheduling() {
        let d = def(1000.0, 100_000);
        let mut inst = VfxInstance::new(0, 7, Vec3::ZERO, Vec3::Z);
        let n = inst.schedule(&d, DT, &LOD, 10);
        assert_eq!(n, 10, "headroom respected exactly");
        drain(&mut inst);
    }

    #[test]
    fn lod_size_compensation_keeps_coverage() {
        assert_eq!(lod_size_compensation(1.0), 1.0);
        assert_eq!(lod_size_compensation(0.0), 1.0);
        let k = lod_size_compensation(0.125);
        assert!((k - 2.0).abs() < 1e-5, "{k}");
        // Between a constant size (coverage drops with the rate) and count × size².
        let half = lod_size_compensation(0.5);
        assert!(half > 1.0 && 0.5 * half * half < 1.0);
        assert_eq!(lod_size_compensation(0.01), LOD_SIZE_CAP);
    }

    #[test]
    fn lod_zero_disables_and_lod_scales_rate() {
        let mut d = def(600.0, 100_000);
        d.system.lod = Some(crate::format::LodDef {
            distances: vec![10.0],
            rate_scale: vec![1.0, 0.0],
        });
        let mut near = VfxInstance::new(0, 1, Vec3::new(5.0, 0.0, 0.0), Vec3::Z);
        let n_near = near.schedule(&d, DT, &LOD, u32::MAX);
        drain(&mut near);
        let mut far = VfxInstance::new(0, 1, Vec3::new(50.0, 0.0, 0.0), Vec3::Z);
        let n_far = far.schedule(&d, DT, &LOD, u32::MAX);
        drain(&mut far);
        assert!(n_near > 0);
        assert_eq!(n_far, 0, "rate_scale 0 disables the far band");
    }

    #[test]
    fn delay_and_duration_are_respected() {
        let mut d = def(600.0, 100_000);
        d.emitters[0].delay = 0.5;
        d.emitters[0].duration = Some(0.25);
        let mut inst = VfxInstance::new(0, 8, Vec3::ZERO, Vec3::Z);
        let early = inst.schedule(&d, DT, &LOD, u32::MAX);
        drain(&mut inst);
        assert_eq!(early, 0);
        let mut during = 0;
        let mut late = 0;
        for i in 0..120 {
            let n = inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
            let t = (i + 2) as f32 * DT;
            if (0.5..0.75).contains(&t) {
                during += n;
            }
            if t > 1.0 {
                late += n;
            }
        }
        assert!(during > 0, "spawns inside the window");
        assert_eq!(late, 0, "no spawns after the window closed");
    }

    #[test]
    fn set_transform_keeps_a_rotation_basis_for_any_heading() {
        let mut inst = VfxInstance::new(0, 1, Vec3::ZERO, Vec3::Z);
        // A slope normal, then the stored up axis itself, then X: no NaN, orthonormal.
        for forward in [Vec3::new(0.0, 0.7, 0.7), Vec3::Y, Vec3::X, Vec3::NEG_Y] {
            inst.set_transform(Vec3::ZERO, forward, 0.0);
            let (right, up, fwd) = inst.axes();
            for v in [right, up, fwd] {
                assert!(v.is_finite() && (v.length() - 1.0).abs() < 1e-4, "{forward}: {v}");
            }
            assert!(right.dot(up).abs() < 1e-4 && up.dot(fwd).abs() < 1e-4 && right.dot(fwd).abs() < 1e-4);
        }
    }

    /// A job scheduled and not yet uploaded is one live particle, not two.
    #[test]
    fn pending_jobs_count_once() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(60);
        let mut inst = VfxInstance::new(0, 11, Vec3::ZERO, Vec3::Z);
        assert_eq!(inst.schedule(&d, DT, &LOD, u32::MAX), 60);
        assert_eq!(inst.pending_jobs(), 60);
        assert_eq!(inst.live_particles(), 60, "pending jobs counted twice");
        assert_eq!(inst.live_alpha(&d), 60, "smoke budget counted pending jobs twice");
        drain(&mut inst);
        assert_eq!(inst.live_particles(), 60, "uploading changes nothing");
    }

    /// A host that schedules several frames before one upload still fills the system budget
    /// (it stopped at half when pending jobs counted twice).
    #[test]
    fn undrained_scheduling_fills_the_budget() {
        let d = def(1000.0, 100); // 16.6 per frame, 0.5 s lifetime
        let mut inst = VfxInstance::new(0, 12, Vec3::ZERO, Vec3::Z);
        for _ in 0..12 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
        }
        assert_eq!(inst.live_particles(), 100, "budget not reached without uploads");
        assert_eq!(inst.pending_jobs(), 100);
    }

    /// Without any upload (a headless host), the pending queue stays within the budget and
    /// scheduling keeps running as particles die, deterministically.
    #[test]
    fn headless_pending_queue_stays_bounded() {
        let d = def(1000.0, 100);
        let run = || {
            let mut inst = VfxInstance::new(0, 13, Vec3::ZERO, Vec3::Z);
            let mut late = 0;
            for frame in 0..600 {
                let n = inst.schedule(&d, DT, &LOD, u32::MAX);
                assert!(
                    inst.pending_jobs() <= 100,
                    "frame {frame}: {} pending",
                    inst.pending_jobs()
                );
                assert!(inst.live_particles() <= 100);
                if frame >= 540 {
                    late += n;
                }
            }
            let mut jobs = Vec::new();
            inst.drain_jobs(&mut jobs);
            (late, jobs)
        };
        let (late, jobs) = run();
        assert!(late > 0, "scheduling stalled after the first lifetime");
        assert_eq!(run().1, jobs, "same inputs, same surviving jobs");
    }

    /// Budgets, LOD and intensity are integer maths on the CPU: two instances with the same
    /// seed driven through the same moving path produce identical jobs, and a different seed
    /// produces different ones with the same count.
    #[test]
    fn moving_instance_jobs_are_reproducible() {
        let mut d = def(240.0, 300);
        d.emitters[0].init.velocity = crate::format::VelocityDef {
            mode: VelocityMode::Directional {
                dir: (0.0, 1.0, 0.0),
                spread: 25.0,
                speed: RangeDef { min: 1.0, max: 3.0 },
            },
            inherit: 0.5,
        };
        d.system.lod = Some(crate::format::LodDef {
            distances: vec![20.0],
            rate_scale: vec![1.0, 0.25],
        });
        let run = |seed: u64| {
            let mut inst = VfxInstance::new(0, seed, Vec3::ZERO, Vec3::Z);
            inst.set_intensity(0.7);
            let mut all = Vec::new();
            for frame in 0..240 {
                let t = frame as f32 * DT;
                // det_sin_cos, not the platform sin: the pinned hash below must not drift for
                // a reason outside the production sampling this test guards
                // (fixrev-22-k, Muse review of fix-vfx1-determinism finding 5).
                let origin = Vec3::new(t * 12.0, 0.0, regia_core::math::det_sin_cos(t * 3.0).0);
                inst.set_transform(origin, Vec3::new(1.0, 0.1, 0.2), DT);
                inst.schedule(&d, DT, &LOD, 250);
                inst.drain_jobs(&mut all);
            }
            all
        };
        let a = run(77);
        assert!(!a.is_empty());
        let bits = |jobs: &[SpawnJob]| -> Vec<u32> {
            jobs.iter()
                .flat_map(|j| {
                    j.position
                        .iter()
                        .chain(&j.velocity)
                        .chain(&j.color)
                        .map(|v| v.to_bits())
                })
                .collect()
        };
        assert_eq!(bits(&a), bits(&run(77)), "same seed, bit-identical jobs");
        // Pinned across platforms: the sampling path uses the deterministic det kernels, so
        // the same seed produces exactly these bits on glibc, the Windows CRT or musl. A
        // platform libm behind f32::sin/cos/tan/cbrt drifts from this hash
        // (fix-vfx1-determinism finding 2).
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for word in bits(&a) {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
        assert_eq!(
            hash, SEED77_JOBS_BITS_HASH,
            "seed 77 jobs drifted from the pinned bits"
        );
        let c = run(78);
        assert_eq!(c.len(), a.len(), "counts do not depend on the seed");
        assert_ne!(bits(&c), bits(&a), "a different seed samples different jobs");
    }

    /// A parent with a death link keeps the instance alive past its own death by the child's
    /// lifetime: the child ring's particles still read the instance's emitter records.
    #[test]
    fn event_children_hold_the_instance_until_they_can_be_dead() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(4); // lifetime 0.5 s
        let mut child = d.emitters[0].clone();
        child.name = "child".into();
        child.spawn.mode = SpawnMode::Event;
        child.spawn.lifetime = RangeDef { min: 2.0, max: 2.0 };
        d.emitters[0].update.events = vec![crate::format::EventSpawnDef {
            on: crate::format::EventTrigger::Death,
            child: "child".into(),
            count: 2,
            probability: 1.0,
            inherit_velocity: 0.0,
        }];
        d.emitters.push(child);
        d.validate().expect("valid");
        let mut inst = VfxInstance::new(0, 3, Vec3::ZERO, Vec3::Z);
        let mut age = 0.0;
        while !inst.is_dead() {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
            age += DT;
            assert!(age < 10.0, "the instance never retired");
        }
        let expected = 0.5 + 2.0 + crate::events::EVENT_HOLD_MARGIN_S;
        assert!(
            (age - expected).abs() < 0.1,
            "retired at {age}, expected about {expected}"
        );
    }

    /// An event-only emitter schedules nothing itself and does not keep a one-shot spawning.
    #[test]
    fn an_event_emitter_schedules_nothing() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Event;
        let mut inst = VfxInstance::new(0, 3, Vec3::ZERO, Vec3::Z);
        assert_eq!(inst.schedule(&d, DT, &LOD, u32::MAX), 0);
        assert!(!inst.is_active());
    }

    #[test]
    fn mixed_lifetimes_retire_on_time() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(300);
        d.emitters[0].spawn.lifetime = RangeDef { min: 0.1, max: 5.0 };
        let mut inst = VfxInstance::new(0, 9, Vec3::ZERO, Vec3::Z);
        let step = 0.25f32;
        inst.schedule(&d, step, &LOD, u32::MAX);
        let mut jobs = Vec::new();
        inst.drain_jobs(&mut jobs);
        assert_eq!(jobs.len(), 300);
        let mut age = step;
        for _ in 0..8 {
            inst.schedule(&d, step, &LOD, u32::MAX);
            age += step;
            let expected = jobs.iter().filter(|j| step + j.lifetime > age).count() as u32;
            assert_eq!(inst.live_particles(), expected, "live count at age {age}");
        }
    }

    /// A few thousand particles with a wide lifetime range retire on the same frames, and the
    /// death bookkeeping of that burst (and of discarding it) stays O(n log n) element moves.
    #[test]
    fn varied_lifetime_burst_death_bookkeeping_is_n_log_n() {
        const N: u32 = 2048;
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(N);
        d.emitters[0].spawn.lifetime = RangeDef { min: 0.1, max: 5.0 };
        reset_death_moves();
        let mut inst = VfxInstance::new(0, 9, Vec3::ZERO, Vec3::Z);
        let step = 0.25f32;
        inst.schedule(&d, step, &LOD, u32::MAX);
        let jobs = drain(&mut inst);
        assert_eq!(jobs.len(), N as usize);
        let distinct = jobs
            .iter()
            .map(|job| job.lifetime.to_bits())
            .collect::<std::collections::HashSet<_>>()
            .len();
        assert!(distinct > 64, "lifetimes did not vary: {distinct}");
        let moves = death_move_count();
        // 2048 * log2(2048) * 4 = 90112. The deque insert of a random rank is about n²/8.
        let bound = u64::from(N) * 11 * 4;
        assert!(
            moves <= bound,
            "spawn element moves {moves} exceed the O(n log n) bound {bound}"
        );

        let mut age = step;
        while age < 6.0 {
            inst.schedule(&d, step, &LOD, u32::MAX);
            age += step;
            let expected = jobs.iter().filter(|job| step + job.lifetime > age).count() as u32;
            assert_eq!(inst.live_particles(), expected, "live count at age {age}");
        }
        assert_eq!(inst.live_particles(), 0, "every particle has retired");

        reset_death_moves();
        let mut stopped = VfxInstance::new(1, 3, Vec3::ZERO, Vec3::Z);
        stopped.schedule(&d, step, &LOD, u32::MAX);
        reset_death_moves();
        stopped.stop();
        stopped.schedule(&d, step, &LOD, u32::MAX);
        assert_eq!(stopped.pending_jobs(), 0);
        assert_eq!(stopped.live_particles(), 0);
        assert_eq!(stopped.death_records().0, 0);
        let discard_moves = death_move_count();
        assert!(
            discard_moves <= bound,
            "discard element moves {discard_moves} exceed the O(n log n) bound {bound}"
        );
    }

    /// `Planar` runs outward in the plane perpendicular to the instance's forward, for any
    /// forward: every velocity points away from the axis through the origin (the same side as
    /// the rotated spawn offset) and carries exactly the authored lift along forward.
    #[test]
    fn planar_velocity_is_outward_in_the_instance_plane() {
        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(400);
        d.emitters[0].init.shape = ShapeDef::Cone {
            angle: 80.0,
            length: RangeDef { min: 0.3, max: 0.6 },
        };
        d.emitters[0].init.velocity = crate::format::VelocityDef {
            mode: VelocityMode::Planar {
                speed: RangeDef { min: 20.0, max: 30.0 },
                lift: RangeDef { min: 1.0, max: 1.0 },
            },
            inherit: 0.0,
        };
        for normal in [Vec3::Y, Vec3::Z, Vec3::new(0.3, 1.0, -0.2).normalize()] {
            let origin = Vec3::new(4.0, -2.0, 7.0);
            let mut inst = VfxInstance::new(0, 5, origin, normal);
            inst.schedule(&d, DT, &LOD, u32::MAX);
            let jobs = drain(&mut inst);
            assert_eq!(jobs.len(), 400);
            for job in &jobs {
                let offset = Vec3::from_array(job.position) - origin;
                let v = Vec3::from_array(job.velocity);
                let along = v.dot(normal);
                assert!((along - 1.0).abs() < 1e-3, "lift {along} for {normal:?}");
                let flat_v = v - normal * along;
                let flat_p = offset - normal * offset.dot(normal);
                assert!(
                    (19.99..=30.01).contains(&flat_v.length()),
                    "outward speed {}",
                    flat_v.length()
                );
                if flat_p.length() > 1e-3 {
                    let cos = flat_v.normalize().dot(flat_p.normalize());
                    assert!(cos > 0.999, "not outward: cos {cos} for {normal:?}");
                }
            }
        }
    }

    /// A parent burst reserves one firing of its event children, and the reservation is gone
    /// once those children can no longer be alive. An event emitter with no parent reserves nothing.
    #[test]
    fn event_child_reservation_tracks_the_parent_not_the_ring() {
        let mut idle = def(0.0, 10_000);
        idle.emitters[0].spawn.mode = SpawnMode::Event;
        let mut lone = VfxInstance::new(0, 1, Vec3::ZERO, Vec3::Z);
        lone.schedule(&idle, DT, &LOD, u32::MAX);
        assert_eq!(lone.event_child_estimate(), (0, 0, 0));

        let mut d = def(0.0, 10_000);
        d.emitters[0].spawn.mode = SpawnMode::Burst(4);
        d.emitters[0].spawn.lifetime = RangeDef { min: 0.5, max: 0.5 };
        let mut child = d.emitters[0].clone();
        child.name = "child".into();
        child.spawn.mode = SpawnMode::Event;
        child.spawn.lifetime = RangeDef { min: 2.0, max: 2.0 };
        d.emitters[0].update.events = vec![crate::format::EventSpawnDef {
            on: crate::format::EventTrigger::Death,
            child: "child".into(),
            count: 2,
            probability: 0.25,
            inherit_velocity: 0.0,
        }];
        d.emitters.push(child);
        d.validate().expect("valid");
        let mut inst = VfxInstance::new(0, 3, Vec3::ZERO, Vec3::Z);
        assert_eq!(inst.event_child_estimate(), (0, 0, 0));
        inst.schedule(&d, DT, &LOD, u32::MAX);
        drain(&mut inst);
        // 4 parents * ceil(2 * 1 * 1 * 1) alpha children. Probability 0.25 still reserves all 8.
        assert_eq!(inst.event_child_estimate(), (8, 0, 0));
        let mut age = DT;
        let mut saw_zero = false;
        for _ in 0..400 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            drain(&mut inst);
            age += DT;
            if inst.event_child_estimate() == (0, 0, 0) {
                saw_zero = true;
                break;
            }
        }
        assert!(saw_zero, "reservation never expired");
        // Parents die at about 0.5 s; the child tail is 2 s. The bound must cover that tail.
        assert!(age > 2.0, "expired too early at {age}");
        assert!(age < 0.5 + 2.0 + 0.2, "held until {age}");
    }

    /// Spec 26 § 3 Init shapes (point, sphere, box, cone): every analytic sample stays inside
    /// its shape, surface variants sit on the boundary, and a box surface point reports the
    /// normal of the face it lies on. 4000 seeded samples per shape, no device.
    #[test]
    fn analytic_spawn_shapes_stay_inside_their_bounds() {
        let none = ShapeSource {
            mesh: None,
            palette: &[],
        };
        let mut rng = Rng::new(26);
        let eps = 1e-4;
        for _ in 0..4000 {
            assert_eq!(sample_analytic(&ShapeDef::Point, &mut rng), Vec3::ZERO);

            let radius = RangeDef { min: 0.5, max: 2.0 };
            let ball = sample_analytic(
                &ShapeDef::Sphere {
                    radius,
                    surface: false,
                },
                &mut rng,
            );
            assert!(ball.length() <= 2.0 + eps, "sphere volume {ball}");
            let shell = sample_analytic(
                &ShapeDef::Sphere {
                    radius,
                    surface: true,
                },
                &mut rng,
            );
            assert!(
                (0.5 - eps..=2.0 + eps).contains(&shell.length()),
                "sphere surface {shell}"
            );

            let h = Vec3::new(1.5, 0.1, 0.75);
            let half_extents = (h.x, h.y, h.z);
            let inside = sample_analytic(
                &ShapeDef::Box {
                    half_extents,
                    surface: false,
                },
                &mut rng,
            );
            assert!(
                inside.abs().cmple(h + Vec3::splat(eps)).all(),
                "box volume {inside}"
            );
            let (face, normal) = sample_shape_local(
                &ShapeDef::Box {
                    half_extents,
                    surface: true,
                },
                &mut rng,
                none,
            );
            assert!(face.abs().cmple(h + Vec3::splat(eps)).all(), "box surface {face}");
            // The normal is a unit axis, and the point touches the half extent on that axis.
            let axis = normal.abs();
            assert!(
                (axis.x + axis.y + axis.z - 1.0).abs() < eps && axis.max_element() == 1.0,
                "{normal}"
            );
            assert!(
                ((face * normal).max_element() - (h * axis).max_element()).abs() < eps,
                "{face} {normal}"
            );

            let length = RangeDef { min: 1.0, max: 3.0 };
            let cone = sample_analytic(&ShapeDef::Cone { angle: 30.0, length }, &mut rng);
            assert!((1.0 - eps..=3.0 + eps).contains(&cone.z), "cone length {cone}");
            let radial = (cone.x * cone.x + cone.y * cone.y).sqrt();
            assert!(
                radial <= cone.z * 30f32.to_radians().tan() + 1e-3,
                "cone radius {cone}"
            );
        }
    }

    /// Collision links reserve the whole class (`u32::MAX`) per parent, until the parent's death
    /// plus the child tail. With parents spawned on different frames, the first reservation's
    /// expiry must not zero the ones still held (a saturated 32-bit total minus `u32::MAX` read
    /// zero, corr-vfx C1). Spawning stops at 0.2 s, so no new parent re-saturates the total: the
    /// parents live 0.5 s and the child tail is 0.5 s, so the reservations expire from about
    /// 1.0 s to 1.2 s and the total must hold until the last one goes.
    #[test]
    fn collision_reservation_survives_the_first_parent_expiry() {
        let mut d = def(60.0, 10_000);
        d.emitters[0].spawn.lifetime = RangeDef { min: 0.5, max: 0.5 };
        let mut child = d.emitters[0].clone();
        child.name = "child".into();
        child.spawn.mode = SpawnMode::Event;
        d.emitters[0].duration = Some(0.2);
        d.emitters[0].update.events = vec![crate::format::EventSpawnDef {
            on: crate::format::EventTrigger::Collision,
            child: "child".into(),
            count: 1,
            probability: 1.0,
            inherit_velocity: 0.0,
        }];
        d.emitters.push(child);
        d.validate().expect("valid");
        let mut inst = VfxInstance::new(0, 5, Vec3::ZERO, Vec3::Z);
        let mut spawned = 0usize;
        let mut age = 0.0f32;
        for frame in 0..120 {
            inst.schedule(&d, DT, &LOD, u32::MAX);
            spawned += drain(&mut inst).len();
            age += DT;
            let (alpha, _, _) = inst.event_child_estimate();
            // The first reservation expires near 1.0 s, the last near 1.2 s.
            if age < 1.15 {
                assert_eq!(
                    alpha,
                    u32::MAX,
                    "frame {frame} (age {age}): held reservations read as {alpha}"
                );
            }
            if age > 1.3 {
                assert_eq!(
                    alpha, 0,
                    "frame {frame} (age {age}): every reservation has expired"
                );
            }
        }
        assert!(spawned > 4, "several parents on different frames: {spawned}");
        assert_eq!(inst.live_particles(), 0, "every parent has died by 2 s");
        assert_eq!(inst.event_child_estimate(), (0, 0, 0));
    }
}
